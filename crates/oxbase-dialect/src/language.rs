//! M6: per-engine SQL language data for code insight (`docs/m6-contracts.md` §4.4): keywords,
//! reserved words, built-in functions with signatures, data types and identifier rules.
//!
//! This is data, not behaviour (PLAN §1 rule 3): each engine crate defines one static
//! [`SqlLanguage`] written from its engine's public documentation and registers it with
//! [`DialectRegistry::register_language`](crate::DialectRegistry::register_language).
//! Completion, diagnostics, hover and the formatter read it; none of them has an engine special
//! case. An engine without its own data gets [`SqlLanguage::standard`], a small SQL-standard
//! core.
//!
//! # Rules for the data (checked by [`SqlLanguage::validate`])
//!
//! - `keywords` and `reserved` are upper-case ASCII, sorted by byte order, without duplicates;
//!   every reserved word is also a keyword. "Reserved" means: cannot be used as a bare
//!   identifier or alias, so completion quotes a catalog name that equals one, and the parser
//!   never takes one for an alias.
//! - `functions` are sorted by [`FunctionInfo::name`] compared ASCII-case-insensitively; one
//!   entry per overload (same name, different `args`). `min_args <= max_args`.
//! - `types` are sorted the same way by [`TypeInfo::name`], without duplicates.
//! - Summaries are one short sentence written for Oxbase (never copied from documentation).

use std::cmp::Ordering;

use oxbase_types::{Engine, IdentCase, ServerVersion};

/// What a built-in function returns per call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FunctionKind {
    /// One value per row.
    Scalar,
    /// One value per group (`count`, `sum`); may also be used as a window function.
    Aggregate,
    /// Only with `OVER (...)` (`row_number`, `lag`).
    Window,
    /// Returns rows; used in `FROM` (`generate_series`, `json_each`).
    Table,
}

/// One built-in function overload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FunctionInfo {
    /// As the engine's documentation writes it (`count`, `GETDATE`).
    pub name: &'static str,
    /// The argument list for display, without parentheses (`text, integer [, integer]`).
    pub args: &'static str,
    /// The return type for display (`bigint`, `same as input`).
    pub returns: &'static str,
    pub kind: FunctionKind,
    /// Fewest arguments a call needs.
    pub min_args: u8,
    /// Most arguments a call takes; `None` = variadic.
    pub max_args: Option<u8>,
    /// One sentence, written for Oxbase.
    pub summary: &'static str,
    /// First server version (major, minor) that has it; `None` = every supported version.
    pub since: Option<(u32, u32)>,
}

impl FunctionInfo {
    /// `true` when server version `v` has this function.
    pub fn available_on(&self, v: &ServerVersion) -> bool {
        self.since
            .is_none_or(|(major, minor)| v.at_least(major, minor))
    }

    /// `true` when a call with `n` arguments fits this overload.
    pub fn accepts(&self, n: usize) -> bool {
        n >= usize::from(self.min_args) && self.max_args.is_none_or(|max| n <= usize::from(max))
    }
}

/// Which parameters a data type takes in parentheses (completion adds a placeholder for them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeParams {
    None,
    /// `varchar(n)`.
    Length,
    /// `float(p)`, `time(p)`.
    Precision,
    /// `numeric(p, s)`.
    PrecisionScale,
}

/// One data type name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeInfo {
    /// As written in DDL (`character varying`, `NVARCHAR`); multi-word names are allowed.
    pub name: &'static str,
    pub params: TypeParams,
    /// One sentence, written for Oxbase.
    pub summary: &'static str,
}

/// The SQL language data of one engine. See the module docs for the rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlLanguage {
    /// The engine this data describes; `None` for [`SqlLanguage::standard`].
    pub engine: Option<Engine>,
    /// Every keyword (upper case, sorted).
    pub keywords: &'static [&'static str],
    /// Reserved words (upper case, sorted, a subset of `keywords`).
    pub reserved: &'static [&'static str],
    pub functions: &'static [FunctionInfo],
    pub types: &'static [TypeInfo],
    /// Characters besides ASCII letters, digits, `_` and non-ASCII letters allowed in a bare
    /// identifier (`$` on PostgreSQL and MySQL; `@ # $` on SQL Server, where a leading `#`
    /// names a temporary table). `$` never starts one.
    pub ident_chars: &'static str,
    /// The schema unqualified names resolve in when the catalog does not say (`public`,
    /// `dbo`, `main`); `None` where the database is the only namespace (MySQL).
    pub default_schema: Option<&'static str>,
}

/// Byte order of `upper` (already upper case) against `probe` upper-cased on the fly.
fn cmp_upper(upper: &str, probe: &str) -> Ordering {
    upper
        .bytes()
        .cmp(probe.bytes().map(|b| b.to_ascii_uppercase()))
}

fn contains_upper(list: &[&str], word: &str) -> bool {
    list.binary_search_by(|k| cmp_upper(k, word)).is_ok()
}

fn sorted_ci(names: impl Iterator<Item = &'static str>) -> Result<(), String> {
    let mut prev: Option<String> = None;
    for name in names {
        let up = name.to_ascii_uppercase();
        if let Some(p) = &prev
            && p.as_str() > up.as_str()
        {
            return Err(format!("`{name}` is out of order"));
        }
        prev = Some(up);
    }
    Ok(())
}

impl SqlLanguage {
    /// The SQL-standard core every engine shares: the most common reserved and non-reserved
    /// keywords, the standard aggregate, window and scalar functions and the standard types.
    /// Used for engines that registered no data of their own.
    pub const fn standard() -> Self {
        Self {
            engine: None,
            keywords: STANDARD_KEYWORDS,
            reserved: STANDARD_RESERVED,
            functions: STANDARD_FUNCTIONS,
            types: STANDARD_TYPES,
            ident_chars: "",
            default_schema: None,
        }
    }

    /// `true` when `word` (any case) is a keyword.
    pub fn is_keyword(&self, word: &str) -> bool {
        contains_upper(self.keywords, word)
    }

    /// `true` when `word` (any case) is a reserved word.
    pub fn is_reserved(&self, word: &str) -> bool {
        contains_upper(self.reserved, word)
    }

    /// Every overload of the built-in function `name` (any case).
    pub fn function(&self, name: &str) -> &'static [FunctionInfo] {
        let functions = self.functions;
        let lo = functions.partition_point(|f| cmp_ci(f.name, name) == Ordering::Less);
        let hi = functions.partition_point(|f| cmp_ci(f.name, name) != Ordering::Greater);
        &functions[lo..hi]
    }

    /// The data type `name` (any case).
    pub fn type_info(&self, name: &str) -> Option<&'static TypeInfo> {
        let types = self.types;
        types
            .binary_search_by(|t| cmp_ci(t.name, name))
            .ok()
            .map(|i| &types[i])
    }

    /// `true` when `name` must be quoted to be read back as the catalog name `name` under
    /// `case`: it is empty, has a character a bare identifier cannot have (see
    /// [`SqlLanguage::ident_chars`]), starts with a digit or `$`, is a reserved word, or its
    /// case would be folded away ([`IdentCase::case_survives_unquoted`]).
    pub fn needs_quotes(&self, name: &str, case: IdentCase) -> bool {
        let extra = |c: char| self.ident_chars.contains(c);
        let mut chars = name.chars();
        let Some(first) = chars.next() else {
            return true;
        };
        let first_ok = first.is_alphabetic() || first == '_' || (extra(first) && first != '$');
        let rest_ok = chars.all(|c| c.is_alphanumeric() || c == '_' || extra(c));
        let bare =
            first_ok && rest_ok && !self.is_reserved(name) && case.case_survives_unquoted(name);
        !bare
    }

    /// Problems with the data (module docs); empty when it is well-formed. Engine crates
    /// assert this is empty in a unit test.
    #[allow(clippy::many_single_char_names)] // short loop and closure names
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for (what, list) in [("keywords", self.keywords), ("reserved", self.reserved)] {
            if let Some(w) = list.iter().find(|w| w.to_ascii_uppercase() != **w) {
                problems.push(format!("{what}: `{w}` is not upper case"));
            }
            for pair in list.windows(2) {
                if let [a, b] = pair
                    && a >= b
                {
                    problems.push(format!(
                        "{what}: `{a}` and `{b}` are out of order or duplicated"
                    ));
                    break;
                }
            }
        }
        for w in self.reserved {
            if !self.is_keyword(w) {
                problems.push(format!("reserved word `{w}` is not in keywords"));
            }
        }
        if let Err(e) = sorted_ci(self.functions.iter().map(|f| f.name)) {
            problems.push(format!("functions: {e}"));
        }
        for f in self.functions {
            if f.max_args.is_some_and(|max| max < f.min_args) {
                problems.push(format!("function `{}`: max_args < min_args", f.name));
            }
            if f.summary.trim().is_empty() {
                problems.push(format!("function `{}`: no summary", f.name));
            }
        }
        if let Err(e) = sorted_ci(self.types.iter().map(|t| t.name)) {
            problems.push(format!("types: {e}"));
        }
        for pair in self.types.windows(2) {
            if let [a, b] = pair
                && a.name.eq_ignore_ascii_case(b.name)
            {
                problems.push(format!("types: `{}` is duplicated", a.name));
                break;
            }
        }
        problems
    }
}

impl Default for SqlLanguage {
    fn default() -> Self {
        Self::standard()
    }
}

/// ASCII-case-insensitive byte order.
fn cmp_ci(a: &str, b: &str) -> Ordering {
    a.bytes()
        .map(|c| c.to_ascii_uppercase())
        .cmp(b.bytes().map(|c| c.to_ascii_uppercase()))
}

// --- the SQL-standard core ------------------------------------------------------------------

const STANDARD_KEYWORDS: &[&str] = &[
    "ACTION",
    "ADD",
    "ALL",
    "ALTER",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BOTH",
    "BY",
    "CASCADE",
    "CASE",
    "CAST",
    "CHECK",
    "COLLATE",
    "COLUMN",
    "COMMIT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "DATA",
    "DATABASE",
    "DECLARE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "ESCAPE",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FETCH",
    "FIRST",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "FUNCTION",
    "GRANT",
    "GROUP",
    "HAVING",
    "IF",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LAST",
    "LEADING",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NATURAL",
    "NO",
    "NOT",
    "NULL",
    "NULLS",
    "OF",
    "OFFSET",
    "ON",
    "ONLY",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRIMARY",
    "PROCEDURE",
    "RANGE",
    "RECURSIVE",
    "REFERENCES",
    "RENAME",
    "REPLACE",
    "RESTRICT",
    "RETURN",
    "RETURNS",
    "RIGHT",
    "ROLLBACK",
    "ROW",
    "ROWS",
    "SCHEMA",
    "SELECT",
    "SEQUENCE",
    "SET",
    "SOME",
    "TABLE",
    "TEMPORARY",
    "THEN",
    "TO",
    "TRAILING",
    "TRANSACTION",
    "TRIGGER",
    "TRUE",
    "TRUNCATE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

const STANDARD_RESERVED: &[&str] = &[
    "ALL",
    "ALTER",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "BETWEEN",
    "BOTH",
    "BY",
    "CASE",
    "CAST",
    "CHECK",
    "COLLATE",
    "COLUMN",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "ESCAPE",
    "EXCEPT",
    "EXISTS",
    "FALSE",
    "FETCH",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "GRANT",
    "GROUP",
    "HAVING",
    "IN",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "LEADING",
    "LEFT",
    "LIKE",
    "NATURAL",
    "NOT",
    "NULL",
    "OF",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "PRIMARY",
    "REFERENCES",
    "RIGHT",
    "SELECT",
    "SET",
    "SOME",
    "TABLE",
    "THEN",
    "TO",
    "TRAILING",
    "TRUE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
    "WITH",
];

const fn func(
    name: &'static str,
    args: &'static str,
    returns: &'static str,
    kind: FunctionKind,
    min_args: u8,
    max_args: Option<u8>,
    summary: &'static str,
) -> FunctionInfo {
    FunctionInfo {
        name,
        args,
        returns,
        kind,
        min_args,
        max_args,
        summary,
        since: None,
    }
}

use FunctionKind::{Aggregate, Scalar, Window};

const STANDARD_FUNCTIONS: &[FunctionInfo] = &[
    func(
        "ABS",
        "n",
        "same as input",
        Scalar,
        1,
        Some(1),
        "Absolute value of a number.",
    ),
    func(
        "AVG",
        "expr",
        "numeric",
        Aggregate,
        1,
        Some(1),
        "Mean of the non-null values in a group.",
    ),
    func(
        "CEILING",
        "n",
        "same as input",
        Scalar,
        1,
        Some(1),
        "Smallest integer not below the number.",
    ),
    func(
        "CHAR_LENGTH",
        "text",
        "integer",
        Scalar,
        1,
        Some(1),
        "Number of characters in a string.",
    ),
    func(
        "COALESCE",
        "value, ...",
        "type of the arguments",
        Scalar,
        1,
        None,
        "First argument that is not null.",
    ),
    func(
        "COUNT",
        "expr | *",
        "integer",
        Aggregate,
        1,
        Some(1),
        "Number of rows, or of non-null values, in a group.",
    ),
    func(
        "DENSE_RANK",
        "",
        "integer",
        Window,
        0,
        Some(0),
        "Rank of the row in its window, without gaps after ties.",
    ),
    func(
        "EXP",
        "n",
        "double precision",
        Scalar,
        1,
        Some(1),
        "e raised to the given power.",
    ),
    func(
        "FLOOR",
        "n",
        "same as input",
        Scalar,
        1,
        Some(1),
        "Largest integer not above the number.",
    ),
    func(
        "LAG",
        "expr [, offset [, default]]",
        "type of expr",
        Window,
        1,
        Some(3),
        "Value from an earlier row of the window.",
    ),
    func(
        "LEAD",
        "expr [, offset [, default]]",
        "type of expr",
        Window,
        1,
        Some(3),
        "Value from a later row of the window.",
    ),
    func(
        "LN",
        "n",
        "double precision",
        Scalar,
        1,
        Some(1),
        "Natural logarithm.",
    ),
    func(
        "LOWER",
        "text",
        "text",
        Scalar,
        1,
        Some(1),
        "The string in lower case.",
    ),
    func(
        "MAX",
        "expr",
        "type of expr",
        Aggregate,
        1,
        Some(1),
        "Largest non-null value in a group.",
    ),
    func(
        "MIN",
        "expr",
        "type of expr",
        Aggregate,
        1,
        Some(1),
        "Smallest non-null value in a group.",
    ),
    func(
        "MOD",
        "a, b",
        "same as input",
        Scalar,
        2,
        Some(2),
        "Remainder of a divided by b.",
    ),
    func(
        "NULLIF",
        "a, b",
        "type of a",
        Scalar,
        2,
        Some(2),
        "Null when both arguments are equal, else the first.",
    ),
    func(
        "POWER",
        "a, b",
        "same as input",
        Scalar,
        2,
        Some(2),
        "a raised to the power b.",
    ),
    func(
        "RANK",
        "",
        "integer",
        Window,
        0,
        Some(0),
        "Rank of the row in its window, with gaps after ties.",
    ),
    func(
        "ROW_NUMBER",
        "",
        "integer",
        Window,
        0,
        Some(0),
        "Position of the row in its window, from 1.",
    ),
    func(
        "SQRT",
        "n",
        "double precision",
        Scalar,
        1,
        Some(1),
        "Square root.",
    ),
    func(
        "SUM",
        "expr",
        "numeric",
        Aggregate,
        1,
        Some(1),
        "Total of the non-null values in a group.",
    ),
    func(
        "UPPER",
        "text",
        "text",
        Scalar,
        1,
        Some(1),
        "The string in upper case.",
    ),
];

const fn ty(name: &'static str, params: TypeParams, summary: &'static str) -> TypeInfo {
    TypeInfo {
        name,
        params,
        summary,
    }
}

const STANDARD_TYPES: &[TypeInfo] = &[
    ty("BIGINT", TypeParams::None, "64-bit integer."),
    ty("BOOLEAN", TypeParams::None, "True, false or null."),
    ty("CHAR", TypeParams::Length, "Fixed-length character string."),
    ty("DATE", TypeParams::None, "Calendar date."),
    ty(
        "DECIMAL",
        TypeParams::PrecisionScale,
        "Exact number with the given precision and scale.",
    ),
    ty(
        "DOUBLE PRECISION",
        TypeParams::None,
        "64-bit floating point number.",
    ),
    ty("FLOAT", TypeParams::Precision, "Floating point number."),
    ty("INTEGER", TypeParams::None, "32-bit integer."),
    ty(
        "NUMERIC",
        TypeParams::PrecisionScale,
        "Exact number with the given precision and scale.",
    ),
    ty("REAL", TypeParams::None, "32-bit floating point number."),
    ty("SMALLINT", TypeParams::None, "16-bit integer."),
    ty("TIME", TypeParams::Precision, "Time of day."),
    ty("TIMESTAMP", TypeParams::Precision, "Date and time of day."),
    ty(
        "VARCHAR",
        TypeParams::Length,
        "Variable-length character string up to the given length.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_standard_data_is_well_formed() {
        let std = SqlLanguage::standard();
        assert_eq!(std.validate(), Vec::<String>::new());
        assert_eq!(SqlLanguage::default(), std);
    }

    #[test]
    fn validate_reports_bad_data() {
        const BAD_FUNCTIONS: &[FunctionInfo] = &[func("b", "", "x", Scalar, 2, Some(1), "")];
        const BAD_TYPES: &[TypeInfo] = &[
            ty("INT", TypeParams::None, "a"),
            ty("int", TypeParams::None, "b"),
        ];
        let bad = SqlLanguage {
            keywords: &["SELECT", "FROM"],
            reserved: &["where"],
            functions: BAD_FUNCTIONS,
            types: BAD_TYPES,
            ..SqlLanguage::standard()
        };
        let problems = bad.validate().join("\n");
        for needle in [
            "out of order",
            "not upper case",
            "not in keywords",
            "max_args < min_args",
            "no summary",
            "duplicated",
        ] {
            assert!(problems.contains(needle), "{needle}: {problems}");
        }
    }

    #[test]
    fn lookups_ignore_case() {
        let std = SqlLanguage::standard();
        assert!(std.is_keyword("select"));
        assert!(std.is_keyword("Limit"));
        assert!(!std.is_reserved("limit"));
        assert!(std.is_reserved("Order"));
        assert!(!std.is_keyword("orders"));
        assert_eq!(std.function("count").len(), 1);
        assert_eq!(std.function("Coalesce")[0].max_args, None);
        assert_eq!(std.function("nope").to_vec(), Vec::<FunctionInfo>::new());
        assert_eq!(
            std.type_info("varchar").map(|t| t.params),
            Some(TypeParams::Length)
        );
        assert!(std.type_info("double precision").is_some());
    }

    #[test]
    fn arity_and_versions() {
        let lag = SqlLanguage::standard().function("lag")[0];
        assert!(lag.accepts(1) && lag.accepts(3));
        assert!(!lag.accepts(0) && !lag.accepts(4));
        let mut f = lag;
        f.since = Some((12, 0));
        assert!(f.available_on(&ServerVersion::new(12, 1, 0)));
        assert!(!f.available_on(&ServerVersion::new(11, 9, 0)));
    }

    #[test]
    fn quoting_needs() {
        let mut lang = SqlLanguage::standard();
        let lower = IdentCase::FoldLower;
        assert!(!lang.needs_quotes("orders", lower));
        assert!(lang.needs_quotes("Orders", lower));
        assert!(!lang.needs_quotes("Orders", IdentCase::Insensitive));
        assert!(lang.needs_quotes("order", lower));
        assert!(lang.needs_quotes("my table", lower));
        assert!(lang.needs_quotes("1st", lower));
        assert!(lang.needs_quotes("", lower));
        assert!(!lang.needs_quotes("straße", lower));
        assert!(lang.needs_quotes("#tmp", IdentCase::Insensitive));
        lang.ident_chars = "@#$";
        assert!(!lang.needs_quotes("#tmp", IdentCase::Insensitive));
        assert!(!lang.needs_quotes("a$b", IdentCase::Insensitive));
        assert!(lang.needs_quotes("$ab", IdentCase::Insensitive));
    }
}
