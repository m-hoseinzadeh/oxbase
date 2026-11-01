//! SQLite language data for code insight (M6, `docs/m6-contracts.md` §4.4): the keywords of
//! SQLite's keyword list, the core, date and time, aggregate, window, math and JSON functions,
//! and the type names SQLite gives an affinity to. Written for Oxbase from the SQLite
//! documentation; summaries are our own. Functions added in recent releases carry the SQLite
//! version that introduced them (`since`).

use oxbase_dialect::{FunctionInfo, FunctionKind, SqlLanguage, TypeInfo, TypeParams};
use oxbase_types::Engine;

use FunctionKind::{Aggregate as A, Scalar as S, Table as T, Window as W};
use TypeParams::{Length as L, None as N, PrecisionScale as PS};

/// The SQLite language data (registered by [`super::register`]).
pub const LANGUAGE: SqlLanguage = SqlLanguage {
    engine: Some(Engine::Sqlite),
    keywords: KEYWORDS,
    reserved: RESERVED,
    functions: FUNCTIONS,
    types: TYPES,
    ident_chars: "$",
    default_schema: Some("main"),
};

/// One function overload available on every supported version.
const fn f(
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

/// One function overload that needs server version `since` (major, minor) or later.
const fn fs(base: FunctionInfo, since: (u32, u32)) -> FunctionInfo {
    FunctionInfo {
        since: Some(since),
        ..base
    }
}

/// One data type.
const fn ty(name: &'static str, params: TypeParams, summary: &'static str) -> TypeInfo {
    TypeInfo {
        name,
        params,
        summary,
    }
}

/// Every keyword (upper case, byte order).
#[rustfmt::skip]
const KEYWORDS: &[&str] = &[
    "ABORT", "ACTION", "ADD", "AFTER", "ALL", "ALTER", "ALWAYS", "ANALYZE", "AND", "AS", "ASC",
    "ATTACH", "AUTOINCREMENT", "BEFORE", "BEGIN", "BETWEEN", "BY", "CASCADE", "CASE", "CAST",
    "CHECK", "COLLATE", "COLUMN", "COMMIT", "CONFLICT", "CONSTRAINT", "CREATE", "CROSS", "CURRENT",
    "CURRENT_DATE", "CURRENT_TIME", "CURRENT_TIMESTAMP", "DATABASE", "DEFAULT", "DEFERRABLE",
    "DEFERRED", "DELETE", "DESC", "DETACH", "DISTINCT", "DO", "DROP", "EACH", "ELSE", "END",
    "ESCAPE", "EXCEPT", "EXCLUDE", "EXCLUSIVE", "EXISTS", "EXPLAIN", "FAIL", "FILTER", "FIRST",
    "FOLLOWING", "FOR", "FOREIGN", "FROM", "FULL", "GENERATED", "GLOB", "GROUP", "GROUPS",
    "HAVING", "IF", "IGNORE", "IMMEDIATE", "IN", "INDEX", "INDEXED", "INITIALLY", "INNER",
    "INSERT", "INSTEAD", "INTERSECT", "INTO", "IS", "ISNULL", "JOIN", "KEY", "LAST", "LEFT",
    "LIKE", "LIMIT", "MATCH", "MATERIALIZED", "NATURAL", "NO", "NOT", "NOTHING", "NOTNULL", "NULL",
    "NULLS", "OF", "OFFSET", "ON", "OR", "ORDER", "OTHERS", "OUTER", "OVER", "PARTITION", "PLAN",
    "PRAGMA", "PRECEDING", "PRIMARY", "QUERY", "RAISE", "RANGE", "RECURSIVE", "REFERENCES",
    "REGEXP", "REINDEX", "RELEASE", "RENAME", "REPLACE", "RESTRICT", "RETURNING", "RIGHT",
    "ROLLBACK", "ROW", "ROWS", "SAVEPOINT", "SELECT", "SET", "TABLE", "TEMP", "TEMPORARY", "THEN",
    "TIES", "TO", "TRANSACTION", "TRIGGER", "UNBOUNDED", "UNION", "UNIQUE", "UPDATE", "USING",
    "VACUUM", "VALUES", "VIEW", "VIRTUAL", "WHEN", "WHERE", "WINDOW", "WITH", "WITHOUT",
];

/// Words that cannot be bare identifiers or aliases.
#[rustfmt::skip]
const RESERVED: &[&str] = &[
    "ADD", "ALL", "ALTER", "AND", "AS", "AUTOINCREMENT", "BETWEEN", "CASE", "CHECK", "COLLATE",
    "COMMIT", "CONSTRAINT", "CREATE", "DEFAULT", "DEFERRABLE", "DELETE", "DISTINCT", "DROP",
    "ELSE", "ESCAPE", "EXCEPT", "EXISTS", "FOREIGN", "FROM", "GROUP", "HAVING", "IN", "INDEX",
    "INSERT", "INTERSECT", "INTO", "IS", "ISNULL", "JOIN", "LIMIT", "NOT", "NOTNULL", "NULL", "ON",
    "OR", "ORDER", "PRIMARY", "REFERENCES", "SELECT", "SET", "TABLE", "THEN", "TO", "TRANSACTION",
    "UNION", "UNIQUE", "UPDATE", "USING", "VALUES", "WHEN", "WHERE",
];

/// Built-in functions, sorted by name ignoring case (one entry per overload).
#[rustfmt::skip]
const FUNCTIONS: &[FunctionInfo] = &[
    f("abs", "x", "same as input", S, 1, Some(1), "Absolute value of a number."),
    fs(f("acos", "x", "real", S, 1, Some(1), "Arc cosine, in radians."), (3, 35)),
    fs(f("acosh", "x", "real", S, 1, Some(1), "Inverse hyperbolic cosine."), (3, 35)),
    fs(f("asin", "x", "real", S, 1, Some(1), "Arc sine, in radians."), (3, 35)),
    fs(f("asinh", "x", "real", S, 1, Some(1), "Inverse hyperbolic sine."), (3, 35)),
    fs(f("atan", "x", "real", S, 1, Some(1), "Arc tangent, in radians."), (3, 35)),
    fs(f("atan2", "y, x", "real", S, 2, Some(2), "Arc tangent of y/x using the signs of both to pick the quadrant."), (3, 35)),
    fs(f("atanh", "x", "real", S, 1, Some(1), "Inverse hyperbolic tangent."), (3, 35)),
    f("avg", "x", "real", A, 1, Some(1), "Mean of the non-null values in a group."),
    fs(f("ceil", "x", "same as input", S, 1, Some(1), "Smallest integer not below x."), (3, 35)),
    fs(f("ceiling", "x", "same as input", S, 1, Some(1), "Smallest integer not below x."), (3, 35)),
    f("changes", "", "integer", S, 0, Some(0), "Rows changed by the last INSERT, UPDATE or DELETE on this connection."),
    f("char", "code, ...", "text", S, 0, None, "String made of the given Unicode code points."),
    f("coalesce", "x, y, ...", "type of the arguments", S, 2, None, "First argument that is not null."),
    fs(f("concat", "x, ...", "text", S, 1, None, "Arguments joined into one string, nulls skipped."), (3, 44)),
    fs(f("concat_ws", "separator, x, ...", "text", S, 2, None, "Arguments joined with a separator, nulls skipped."), (3, 44)),
    fs(f("cos", "x", "real", S, 1, Some(1), "Cosine of an angle in radians."), (3, 35)),
    fs(f("cosh", "x", "real", S, 1, Some(1), "Hyperbolic cosine."), (3, 35)),
    f("count", "x or *", "integer", A, 0, Some(1), "Number of rows, or of non-null values, in a group."),
    f("cume_dist", "", "real", W, 0, Some(0), "Share of the window's rows up to and including this row's peers."),
    f("date", "time-value, modifier, ...", "text", S, 0, None, "Date as YYYY-MM-DD after applying the modifiers."),
    f("datetime", "time-value, modifier, ...", "text", S, 0, None, "Date and time as YYYY-MM-DD HH:MM:SS after applying the modifiers."),
    fs(f("degrees", "x", "real", S, 1, Some(1), "Radians converted to degrees."), (3, 35)),
    f("dense_rank", "", "integer", W, 0, Some(0), "Rank of the row in its window, without gaps after ties."),
    fs(f("exp", "x", "real", S, 1, Some(1), "e raised to the given power."), (3, 35)),
    f("first_value", "expr", "type of expr", W, 1, Some(1), "Value from the first row of the window frame."),
    fs(f("floor", "x", "same as input", S, 1, Some(1), "Largest integer not above x."), (3, 35)),
    f("format", "format, ...", "text", S, 1, None, "Text built from a printf-style format string."),
    f("glob", "pattern, string", "integer", S, 2, Some(2), "1 when the string matches the case-sensitive GLOB pattern."),
    f("group_concat", "x, separator", "text", A, 1, Some(2), "Non-null values of a group joined into one string."),
    f("hex", "x", "text", S, 1, Some(1), "Upper-case hexadecimal rendering of a value's bytes."),
    f("ifnull", "x, y", "type of the arguments", S, 2, Some(2), "The first argument unless it is null, else the second."),
    f("iif", "condition, then, else", "type of the results", S, 3, Some(3), "The second argument when the condition is true, else the third."),
    f("instr", "string, substring", "integer", S, 2, Some(2), "1-based position of the first occurrence of a substring, 0 if absent."),
    f("json", "json", "text", S, 1, Some(1), "Minified JSON text after checking it is well formed."),
    fs(f("jsonb", "json", "blob", S, 1, Some(1), "JSON converted to SQLite's binary JSON format."), (3, 45)),
    f("json_array", "value, ...", "text", S, 0, None, "JSON array made of the arguments."),
    f("json_array_length", "json, path", "integer", S, 1, Some(2), "Number of elements in a JSON array."),
    f("json_each", "json, path", "table", T, 1, Some(2), "One row per element or member of a JSON value."),
    fs(f("json_error_position", "json", "integer", S, 1, Some(1), "Position of the first syntax error in JSON text, 0 if none."), (3, 42)),
    f("json_extract", "json, path, ...", "any", S, 1, None, "Values found at the given paths of a JSON document."),
    f("json_group_array", "value", "text", A, 1, Some(1), "JSON array of the values of a group."),
    f("json_group_object", "name, value", "text", A, 2, Some(2), "JSON object built from the name and value pairs of a group."),
    f("json_insert", "json, path, value, ...", "text", S, 1, None, "JSON with values inserted where the paths do not exist yet."),
    f("json_object", "name, value, ...", "text", S, 0, None, "JSON object made of name and value pairs."),
    f("json_patch", "target, patch", "text", S, 2, Some(2), "JSON object after applying a merge patch."),
    fs(f("json_pretty", "json, indent", "text", S, 1, Some(2), "JSON text laid out on several lines."), (3, 46)),
    f("json_quote", "value", "text", S, 1, Some(1), "A SQL value as JSON text."),
    f("json_remove", "json, path, ...", "text", S, 1, None, "JSON with the elements at the given paths removed."),
    f("json_replace", "json, path, value, ...", "text", S, 1, None, "JSON with existing values at the paths replaced."),
    f("json_set", "json, path, value, ...", "text", S, 1, None, "JSON with values at the paths inserted or replaced."),
    f("json_tree", "json, path", "table", T, 1, Some(2), "One row per element of a JSON value, walking it recursively."),
    f("json_type", "json, path", "text", S, 1, Some(2), "Type of a JSON value: null, true, false, integer, real, text, array or object."),
    f("json_valid", "json, flags", "integer", S, 1, Some(2), "1 when the argument is well-formed JSON."),
    f("julianday", "time-value, modifier, ...", "real", S, 0, None, "Julian day number after applying the modifiers."),
    f("lag", "expr, offset, default", "type of expr", W, 1, Some(3), "Value from an earlier row of the window."),
    f("last_insert_rowid", "", "integer", S, 0, Some(0), "Rowid of the last row inserted on this connection."),
    f("last_value", "expr", "type of expr", W, 1, Some(1), "Value from the last row of the window frame."),
    f("lead", "expr, offset, default", "type of expr", W, 1, Some(3), "Value from a later row of the window."),
    f("length", "x", "integer", S, 1, Some(1), "Number of characters of a string, or bytes of a blob."),
    f("like", "pattern, string, escape", "integer", S, 2, Some(3), "1 when the string matches the LIKE pattern."),
    f("likelihood", "x, probability", "same as input", S, 2, Some(2), "Its first argument; tells the planner how likely it is to be true."),
    f("likely", "x", "same as input", S, 1, Some(1), "Its argument; tells the planner it is probably true."),
    fs(f("ln", "x", "real", S, 1, Some(1), "Natural logarithm."), (3, 35)),
    f("load_extension", "file, entry", "null", S, 1, Some(2), "Loads an extension library into the connection."),
    fs(f("log", "x", "real", S, 1, Some(1), "Base-10 logarithm."), (3, 35)),
    fs(f("log", "base, x", "real", S, 2, Some(2), "Logarithm of x to the given base."), (3, 35)),
    fs(f("log10", "x", "real", S, 1, Some(1), "Base-10 logarithm."), (3, 35)),
    fs(f("log2", "x", "real", S, 1, Some(1), "Base-2 logarithm."), (3, 35)),
    f("lower", "text", "text", S, 1, Some(1), "The string in lower case (ASCII letters only by default)."),
    f("ltrim", "text, characters", "text", S, 1, Some(2), "The string without the given characters (spaces by default) on the left."),
    f("max", "x", "type of x", A, 1, Some(1), "Largest non-null value in a group."),
    f("max", "x, y, ...", "type of the arguments", S, 2, None, "Largest of the arguments."),
    f("min", "x", "type of x", A, 1, Some(1), "Smallest non-null value in a group."),
    f("min", "x, y, ...", "type of the arguments", S, 2, None, "Smallest of the arguments."),
    fs(f("mod", "x, y", "real", S, 2, Some(2), "Remainder of x divided by y."), (3, 35)),
    f("nth_value", "expr, n", "type of expr", W, 2, Some(2), "Value from the n-th row of the window frame."),
    f("ntile", "n", "integer", W, 1, Some(1), "Bucket number from 1 to n that the row falls into."),
    f("nullif", "x, y", "type of x", S, 2, Some(2), "Null when both arguments are equal, else the first."),
    fs(f("octet_length", "x", "integer", S, 1, Some(1), "Number of bytes of the value's text or blob form."), (3, 43)),
    f("percent_rank", "", "real", W, 0, Some(0), "Relative rank of the row, from 0 to 1."),
    fs(f("pi", "", "real", S, 0, Some(0), "The number pi."), (3, 35)),
    fs(f("pow", "x, y", "real", S, 2, Some(2), "x raised to the power y."), (3, 35)),
    fs(f("power", "x, y", "real", S, 2, Some(2), "x raised to the power y."), (3, 35)),
    f("printf", "format, ...", "text", S, 1, None, "Text built from a printf-style format string."),
    f("quote", "x", "text", S, 1, Some(1), "The value as a SQL literal."),
    fs(f("radians", "x", "real", S, 1, Some(1), "Degrees converted to radians."), (3, 35)),
    f("random", "", "integer", S, 0, Some(0), "Pseudo-random 64-bit integer."),
    f("randomblob", "n", "blob", S, 1, Some(1), "Blob of n pseudo-random bytes."),
    f("rank", "", "integer", W, 0, Some(0), "Rank of the row in its window, with gaps after ties."),
    f("replace", "text, from, to", "text", S, 3, Some(3), "The string with every occurrence of one substring replaced by another."),
    f("round", "x, digits", "real", S, 1, Some(2), "Number rounded to the given decimal places (0 by default)."),
    f("row_number", "", "integer", W, 0, Some(0), "Position of the row in its window, from 1."),
    f("rtrim", "text, characters", "text", S, 1, Some(2), "The string without the given characters (spaces by default) on the right."),
    fs(f("sign", "x", "integer", S, 1, Some(1), "-1, 0 or 1 for a negative, zero or positive number."), (3, 35)),
    fs(f("sin", "x", "real", S, 1, Some(1), "Sine of an angle in radians."), (3, 35)),
    fs(f("sinh", "x", "real", S, 1, Some(1), "Hyperbolic sine."), (3, 35)),
    f("soundex", "text", "text", S, 1, Some(1), "Soundex code of a string (when compiled in)."),
    f("sqlite_compileoption_get", "n", "text", S, 1, Some(1), "The n-th compile-time option."),
    f("sqlite_compileoption_used", "name", "integer", S, 1, Some(1), "1 when the library was built with the option."),
    f("sqlite_offset", "column", "integer", S, 1, Some(1), "File offset of the record holding the column value."),
    f("sqlite_source_id", "", "text", S, 0, Some(0), "Check-in identifier of the SQLite source."),
    f("sqlite_version", "", "text", S, 0, Some(0), "Version of the SQLite library."),
    fs(f("sqrt", "x", "real", S, 1, Some(1), "Square root."), (3, 35)),
    f("strftime", "format, time-value, modifier, ...", "text", S, 1, None, "Date and time formatted with strftime-style substitutions."),
    fs(f("string_agg", "x, separator", "text", A, 2, Some(2), "Non-null values of a group joined with a separator."), (3, 44)),
    f("substr", "text, start, length", "text", S, 2, Some(3), "Part of a string from a 1-based position."),
    f("substring", "text, start, length", "text", S, 2, Some(3), "Part of a string from a 1-based position."),
    f("sum", "x", "numeric", A, 1, Some(1), "Total of the non-null values in a group (null for none)."),
    fs(f("tan", "x", "real", S, 1, Some(1), "Tangent of an angle in radians."), (3, 35)),
    fs(f("tanh", "x", "real", S, 1, Some(1), "Hyperbolic tangent."), (3, 35)),
    f("time", "time-value, modifier, ...", "text", S, 0, None, "Time as HH:MM:SS after applying the modifiers."),
    fs(f("timediff", "time-value, time-value", "text", S, 2, Some(2), "Difference between two times as a signed duration string."), (3, 43)),
    f("total", "x", "real", A, 1, Some(1), "Total of the non-null values in a group (0.0 for none)."),
    f("total_changes", "", "integer", S, 0, Some(0), "Rows changed on this connection since it was opened."),
    f("trim", "text, characters", "text", S, 1, Some(2), "The string without the given characters (spaces by default) at both ends."),
    fs(f("trunc", "x", "same as input", S, 1, Some(1), "Number with its fractional part removed."), (3, 35)),
    f("typeof", "x", "text", S, 1, Some(1), "Storage class of a value: null, integer, real, text or blob."),
    fs(f("unhex", "text, ignore", "blob", S, 1, Some(2), "Blob from hexadecimal text."), (3, 41)),
    f("unicode", "text", "integer", S, 1, Some(1), "Code point of the first character of a string."),
    fs(f("unixepoch", "time-value, modifier, ...", "integer", S, 0, None, "Seconds since 1970-01-01 after applying the modifiers."), (3, 38)),
    f("unlikely", "x", "same as input", S, 1, Some(1), "Its argument; tells the planner it is probably false."),
    f("upper", "text", "text", S, 1, Some(1), "The string in upper case (ASCII letters only by default)."),
    f("zeroblob", "n", "blob", S, 1, Some(1), "Blob of n zero bytes."),
];

/// Built-in data types, sorted by name ignoring case.
#[rustfmt::skip]
const TYPES: &[TypeInfo] = &[
    ty("ANY", N, "Any value, kept as given (STRICT tables)."),
    ty("BIGINT", N, "Integer affinity."),
    ty("BLOB", N, "Bytes stored as given."),
    ty("BOOLEAN", N, "Numeric affinity; true and false are 1 and 0."),
    ty("CHARACTER", L, "Text affinity."),
    ty("CLOB", N, "Text affinity."),
    ty("DATE", N, "Numeric affinity; dates are usually stored as text or numbers."),
    ty("DATETIME", N, "Numeric affinity; timestamps are usually stored as text or numbers."),
    ty("DECIMAL", PS, "Numeric affinity."),
    ty("DOUBLE", N, "Real affinity."),
    ty("DOUBLE PRECISION", N, "Real affinity."),
    ty("FLOAT", N, "Real affinity."),
    ty("INT", N, "Integer affinity."),
    ty("INT2", N, "Integer affinity."),
    ty("INT8", N, "Integer affinity."),
    ty("INTEGER", N, "Integer affinity; INTEGER PRIMARY KEY is the rowid."),
    ty("MEDIUMINT", N, "Integer affinity."),
    ty("NATIVE CHARACTER", L, "Text affinity."),
    ty("NCHAR", L, "Text affinity."),
    ty("NUMERIC", PS, "Numeric affinity."),
    ty("NVARCHAR", L, "Text affinity."),
    ty("REAL", N, "8-byte floating point number."),
    ty("SMALLINT", N, "Integer affinity."),
    ty("TEXT", N, "Text in the database encoding."),
    ty("TINYINT", N, "Integer affinity."),
    ty("UNSIGNED BIG INT", N, "Integer affinity."),
    ty("VARCHAR", L, "Text affinity; the length is not enforced."),
    ty("VARYING CHARACTER", L, "Text affinity."),
];

#[cfg(test)]
mod tests {
    use oxbase_dialect::DialectRegistry;

    use super::*;

    #[test]
    fn the_data_is_well_formed_and_registered() {
        assert_eq!(LANGUAGE.validate(), Vec::<String>::new());
        assert!(
            LANGUAGE.functions.len() >= 80,
            "{}",
            LANGUAGE.functions.len()
        );
        assert!(LANGUAGE.is_reserved("select"));
        assert!(LANGUAGE.is_keyword("pragma"));
        assert!(!LANGUAGE.is_reserved("rowid"));
        assert_eq!(LANGUAGE.function("json_extract").len(), 1);
        assert_eq!(LANGUAGE.function("max").len(), 2);
        assert!(LANGUAGE.type_info("integer").is_some());
        let mut reg = DialectRegistry::new();
        super::super::register(&mut reg);
        assert!(reg.has_language(Engine::Sqlite));
        let problems: Vec<String> = reg
            .missing_insight_roles(Engine::Sqlite)
            .into_iter()
            .filter(|p| p.contains("SqlLanguage"))
            .collect();
        assert_eq!(problems, Vec::<String>::new());
    }
}
