//! The parser's grammar words: which words start clauses, join relations, combine operands and
//! so on. This is SQL *structure*, the same for every dialect: each list holds the union of the
//! five dialects' spellings (a word a dialect lacks is simply never met there). Which words an
//! engine reserves comes from its `SqlLanguage`, never from here.
//!
//! Lists searched with [`contains`] are sorted upper-case ASCII without duplicates (checked by a
//! unit test). Lists offered to completion ([`crate::CursorContext::keywords`]) are in "most
//! likely first" order instead.

use std::cmp::Ordering;

use crate::outline::Clause;

/// `true` when `word` (any case) is in `list` (sorted upper case).
pub(crate) fn contains(list: &[&str], word: &str) -> bool {
    list.binary_search_by(|k| cmp_upper(k, word)).is_ok()
}

/// Byte order of `upper` (already upper case) against `probe` upper-cased on the fly.
fn cmp_upper(upper: &str, probe: &str) -> Ordering {
    upper
        .bytes()
        .cmp(probe.bytes().map(|b| b.to_ascii_uppercase()))
}

// ---------------------------------------------------------------------------------------------
// Sorted lists (binary search).
// ---------------------------------------------------------------------------------------------

/// Words that end an expression and are never a bare alias or name: clause and statement
/// keywords of any dialect.
pub(crate) const STOP: &[&str] = &[
    "ALTER",
    "AS",
    "BEGIN",
    "CALL",
    "CREATE",
    "CROSS",
    "DECLARE",
    "DELETE",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXEC",
    "EXECUTE",
    "FETCH",
    "FOR",
    "FROM",
    "FULL",
    "GO",
    "GROUP",
    "HAVING",
    "IF",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "JOIN",
    "LATERAL",
    "LEFT",
    "LIMIT",
    "LOCK",
    "MERGE",
    "MINUS",
    "NATURAL",
    "OFFSET",
    "ON",
    "OPTION",
    "ORDER",
    "OUTER",
    "OUTPUT",
    "PARTITION",
    "PIVOT",
    "PRINT",
    "QUALIFY",
    "RETURN",
    "RETURNING",
    "RIGHT",
    "SELECT",
    "SET",
    "STRAIGHT_JOIN",
    "TABLESAMPLE",
    "THEN",
    "TRUNCATE",
    "UNION",
    "UNPIVOT",
    "UPDATE",
    "USE",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
    "WHILE",
    "WINDOW",
    "WITH",
];

/// Stop words that are also functions when `(` follows (`LEFT(x, 2)`, MySQL `IF(c, a, b)`,
/// `VALUES(col)`, `INSERT(s, ...)`, `TRUNCATE(n, d)`).
pub(crate) const STOP_FUNCTIONS: &[&str] = &["IF", "INSERT", "LEFT", "RIGHT", "TRUNCATE", "VALUES"];

/// Infix operator words (after an operand).
pub(crate) const OPERATORS: &[&str] = &[
    "AND", "BETWEEN", "DIV", "ESCAPE", "GLOB", "ILIKE", "IN", "IS", "LIKE", "MATCH", "MEMBER",
    "MOD", "NOT", "OR", "OVERLAPS", "REGEXP", "RLIKE", "SIMILAR", "SOUNDS", "XOR",
];

/// Words that may follow an operator word (`NOT IN`, `IS NOT`, `SIMILAR TO`, `MEMBER OF`).
pub(crate) const OPERATOR_CONT: &[&str] = &[
    "BETWEEN", "DISTINCT", "GLOB", "ILIKE", "IN", "LIKE", "MATCH", "NOT", "OF", "REGEXP", "RLIKE",
    "SIMILAR", "TO",
];

/// Prefix words before an operand (`NOT x`, `EXISTS (...)`, `= ANY (...)`, `ARRAY[...]`).
pub(crate) const PREFIX: &[&str] = &[
    "ALL", "ANY", "ARRAY", "BINARY", "DISTINCT", "EXISTS", "NOT", "PRIOR", "SOME",
];

/// Keywords that are values on their own (never a column reference).
pub(crate) const VALUE_WORDS: &[&str] = &[
    "CURRENT_CATALOG",
    "CURRENT_DATE",
    "CURRENT_ROLE",
    "CURRENT_SCHEMA",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "DEFAULT",
    "FALSE",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "NULL",
    "SESSION_USER",
    "SYSTEM_USER",
    "TRUE",
    "UNKNOWN",
];

/// Type names that start a typed literal (`DATE '2020-01-01'`) besides the language's types.
pub(crate) const TYPED_LITERALS: &[&str] =
    &["DATE", "DATETIME", "TIME", "TIMESTAMP", "TIMESTAMPTZ"];

/// Units after an `INTERVAL` value (`INTERVAL '1' DAY`, `INTERVAL 2 HOUR_MINUTE`,
/// `DAY TO SECOND`).
pub(crate) const INTERVAL_UNITS: &[&str] = &[
    "DAY",
    "DAY_HOUR",
    "DAY_MICROSECOND",
    "DAY_MINUTE",
    "DAY_SECOND",
    "HOUR",
    "HOUR_MICROSECOND",
    "HOUR_MINUTE",
    "HOUR_SECOND",
    "MICROSECOND",
    "MILLISECOND",
    "MINUTE",
    "MINUTE_MICROSECOND",
    "MINUTE_SECOND",
    "MONTH",
    "QUARTER",
    "SECOND",
    "SECOND_MICROSECOND",
    "TO",
    "WEEK",
    "YEAR",
    "YEAR_MONTH",
];

/// Words that continue a data type name (`double precision`, `character varying`,
/// `timestamp with time zone`, `int unsigned`).
pub(crate) const TYPE_CONT: &[&str] = &[
    "LOCAL",
    "PRECISION",
    "SIGNED",
    "TIME",
    "UNSIGNED",
    "VARYING",
    "WITH",
    "WITHOUT",
    "ZEROFILL",
    "ZONE",
];

/// Statement keywords that end the previous statement in a text whose statements need no
/// terminator (T-SQL batches).
pub(crate) const STRONG: &[&str] = &[
    "ALTER",
    "BEGIN",
    "CALL",
    "CREATE",
    "DECLARE",
    "DELETE",
    "DROP",
    "ELSE",
    "END",
    "EXEC",
    "EXECUTE",
    "IF",
    "INSERT",
    "MERGE",
    "PRINT",
    "RAISERROR",
    "RETURN",
    "SELECT",
    "SET",
    "THROW",
    "TRUNCATE",
    "UPDATE",
    "USE",
    "WHILE",
    "WITH",
];

/// Functions whose first argument is a field keyword, not a column (`EXTRACT(YEAR FROM d)`,
/// `DATEADD(day, 1, d)`, `TIMESTAMPDIFF(MONTH, a, b)`).
pub(crate) const FIELD_FIRST: &[&str] = &[
    "DATEADD",
    "DATEDIFF",
    "DATEDIFF_BIG",
    "DATENAME",
    "DATEPART",
    "DATETRUNC",
    "DATE_BUCKET",
    "EXTRACT",
    "TIMESTAMPADD",
    "TIMESTAMPDIFF",
];

/// Modifiers between `SELECT` and its list (besides `DISTINCT [ON]` and `TOP`).
pub(crate) const SELECT_MODIFIERS: &[&str] = &[
    "ALL",
    "DISTINCTROW",
    "HIGH_PRIORITY",
    "SQL_BIG_RESULT",
    "SQL_BUFFER_RESULT",
    "SQL_CACHE",
    "SQL_CALC_FOUND_ROWS",
    "SQL_NO_CACHE",
    "SQL_SMALL_RESULT",
    "STRAIGHT_JOIN",
];

/// Words that start a join.
pub(crate) const JOIN_WORDS: &[&str] = &[
    "CROSS",
    "FULL",
    "INNER",
    "JOIN",
    "LEFT",
    "NATURAL",
    "OUTER",
    "RIGHT",
    "STRAIGHT_JOIN",
];

/// Words before `JOIN` / `APPLY`.
pub(crate) const JOIN_MODIFIERS: &[&str] = &[
    "CROSS", "FULL", "INNER", "LEFT", "NATURAL", "OUTER", "RIGHT",
];

/// Words of `LIMIT` / `OFFSET` / `FETCH` clauses besides their values.
pub(crate) const LIMIT_WORDS: &[&str] = &[
    "ALL", "FIRST", "NEXT", "ONLY", "PERCENT", "ROW", "ROWS", "TIES", "WITH",
];

/// Words after an `ORDER BY` item.
pub(crate) const ORDER_SUFFIX: &[&str] = &["ASC", "DESC", "FIRST", "LAST", "NULLS"];

/// Window frame starts (`ROWS BETWEEN ...`).
pub(crate) const FRAME: &[&str] = &["GROUPS", "RANGE", "ROWS"];

/// Modifiers between `CREATE` and the object word.
pub(crate) const CREATE_MODIFIERS: &[&str] = &[
    "CLUSTERED",
    "CONSTRAINT",
    "EXTERNAL",
    "FULLTEXT",
    "GLOBAL",
    "LOCAL",
    "MATERIALIZED",
    "NONCLUSTERED",
    "RECURSIVE",
    "SPATIAL",
    "TEMP",
    "TEMPORARY",
    "UNIQUE",
    "UNLOGGED",
    "VIRTUAL",
];

/// Words that start a table constraint inside `CREATE TABLE (...)` / after `ALTER TABLE ADD`.
pub(crate) const TABLE_CONSTRAINTS: &[&str] = &[
    "CHECK",
    "CONSTRAINT",
    "EXCLUDE",
    "FOREIGN",
    "FULLTEXT",
    "PRIMARY",
    "SPATIAL",
    "UNIQUE",
];

/// Parameter modes of a routine.
pub(crate) const PARAM_MODES: &[&str] = &["IN", "INOUT", "OUT", "VARIADIC"];

/// Words that start a routine or trigger body.
pub(crate) const BODY_START: &[&str] = &[
    "BEGIN", "CALL", "DECLARE", "DELETE", "IF", "INSERT", "RETURN", "SELECT", "SET", "UPDATE",
    "WHILE",
];

/// Words of a `MERGE ... WHEN` condition.
pub(crate) const MERGE_MATCH: &[&str] = &["BY", "MATCHED", "NOT", "SOURCE", "TARGET"];

/// Modifiers of `INSERT`, `UPDATE` and `DELETE` (MySQL).
pub(crate) const DML_MODIFIERS: &[&str] = &[
    "DELAYED",
    "HIGH_PRIORITY",
    "IGNORE",
    "LOW_PRIORITY",
    "QUICK",
];

/// Words after `BEGIN` that make it a transaction statement (not a block).
pub(crate) const TRANSACTION_NEXT: &[&str] = &[
    "DEFERRED",
    "DISTRIBUTED",
    "EXCLUSIVE",
    "IMMEDIATE",
    "ISOLATION",
    "READ",
    "TRAN",
    "TRANSACTION",
    "WORK",
];

/// Words after `END` that close a block of that kind.
pub(crate) const END_SUFFIX: &[&str] = &[
    "CASE", "CATCH", "FOR", "IF", "LOOP", "REPEAT", "TRY", "WHILE",
];

/// Statements read as a keyword and the rest skipped (subqueries still parsed).
pub(crate) const SKIPPED_STATEMENTS: &[&str] = &[
    "ANALYZE",
    "CHECKPOINT",
    "CLOSE",
    "CLUSTER",
    "DEALLOCATE",
    "DISCARD",
    "DO",
    "FETCH",
    "GOTO",
    "HANDLER",
    "ITERATE",
    "KILL",
    "LEAVE",
    "LISTEN",
    "LOAD",
    "LOOP",
    "NOTIFY",
    "OPEN",
    "OPTIMIZE",
    "PREPARE",
    "PRINT",
    "RAISERROR",
    "REINDEX",
    "REPEAT",
    "RESIGNAL",
    "SIGNAL",
    "THROW",
    "VACUUM",
    "WAITFOR",
];

/// Statements that are a condition followed by other statements (`IF x THEN`, T-SQL
/// `IF x BEGIN`, `WHILE x DO`).
pub(crate) const CONDITIONS: &[&str] = &["ELSEIF", "ELSIF", "IF", "UNTIL", "WHEN", "WHILE"];

/// Transaction control statements.
pub(crate) const TRANSACTIONS: &[&str] = &[
    "ABORT",
    "COMMIT",
    "RELEASE",
    "ROLLBACK",
    "SAVEPOINT",
    "START",
];

/// Session statements (`StatementClass::Set`).
pub(crate) const SESSION: &[&str] = &["PRAGMA", "RESET", "SET", "SHOW"];

/// Words after `ALTER TABLE t DROP` that do not name a column.
pub(crate) const DROP_NON_COLUMNS: &[&str] = &[
    "CHECK",
    "CONSTRAINT",
    "FOREIGN",
    "INDEX",
    "KEY",
    "PARTITION",
    "PRIMARY",
];

/// Words of an `ALTER TABLE ... ALTER COLUMN c <here>` action that are not a type.
pub(crate) const ALTER_COLUMN_ACTIONS: &[&str] = &["ADD", "DROP", "OPTIONS", "RESET", "SET"];

// ---------------------------------------------------------------------------------------------
// Completion lists (most likely first).
// ---------------------------------------------------------------------------------------------

/// Statement starts.
pub(crate) const STATEMENT_STARTERS: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "WITH",
    "CREATE",
    "ALTER",
    "DROP",
    "TRUNCATE",
    "MERGE",
    "CALL",
    "EXEC",
    "EXECUTE",
    "EXPLAIN",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "SET",
    "SHOW",
    "USE",
    "GRANT",
    "REVOKE",
    "VALUES",
    "DECLARE",
    "DESCRIBE",
    "PRAGMA",
    "REPLACE",
    "START",
    "SAVEPOINT",
    "ANALYZE",
    "VACUUM",
];

/// At the start of an operand.
pub(crate) const EXPR_START: &[&str] = &[
    "CASE", "NOT", "EXISTS", "NULL", "TRUE", "FALSE", "CAST", "INTERVAL",
];

/// At the start of a select list.
pub(crate) const SELECT_START: &[&str] = &[
    "DISTINCT", "ALL", "TOP", "CASE", "NOT", "EXISTS", "NULL", "CAST",
];

/// Where a query starts (after `UNION`, `AS` of a view).
pub(crate) const QUERY_START: &[&str] = &["SELECT", "VALUES", "WITH"];

/// After the CTEs of a `WITH`.
pub(crate) const AFTER_WITH: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE", "MERGE"];

/// After a CTE name (and its column list).
pub(crate) const AFTER_CTE_NAME: &[&str] = &["AS"];

/// After a select-list item.
pub(crate) const SELECT_ITEM_AFTER: &[&str] = &[
    "FROM", "AS", "AND", "OR", "IS", "NOT", "IN", "LIKE", "BETWEEN", "OVER", "INTO", "WHERE",
    "GROUP", "ORDER", "LIMIT", "UNION",
];

/// After an operand of a `WHERE` condition.
const WHERE_AFTER: &[&str] = &[
    "AND",
    "OR",
    "IS",
    "NOT",
    "IN",
    "LIKE",
    "BETWEEN",
    "ILIKE",
    "GROUP",
    "ORDER",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "UNION",
    "EXCEPT",
    "INTERSECT",
];

/// After an operand of a join condition.
const JOIN_ON_AFTER: &[&str] = &[
    "AND", "OR", "IS", "NOT", "IN", "LIKE", "BETWEEN", "JOIN", "INNER", "LEFT", "RIGHT", "FULL",
    "CROSS", "WHERE", "GROUP", "ORDER", "LIMIT",
];

/// After an operand of `HAVING`.
const HAVING_AFTER: &[&str] = &[
    "AND", "OR", "IS", "NOT", "IN", "LIKE", "BETWEEN", "ORDER", "LIMIT", "UNION",
];

/// After a `GROUP BY` item.
const GROUP_AFTER: &[&str] = &[
    "HAVING", "ORDER", "LIMIT", "WITH", "ROLLUP", "WINDOW", "UNION",
];

/// After an `ORDER BY` item.
pub(crate) const ORDER_AFTER: &[&str] =
    &["ASC", "DESC", "NULLS", "LIMIT", "OFFSET", "FETCH", "FOR"];

/// After a `LIMIT` value.
const LIMIT_AFTER: &[&str] = &["OFFSET", "ROWS", "ONLY", "FOR", "UNION"];

/// After any other operand.
const OPERATOR_AFTER: &[&str] = &["AND", "OR", "IS", "NOT", "IN", "LIKE", "BETWEEN"];

/// After the relations of a `FROM` list (an alias already given).
pub(crate) const FROM_TAIL: &[&str] = &[
    "WHERE",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "NATURAL",
    "OUTER",
    "GROUP",
    "HAVING",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "WINDOW",
    "FOR",
];

/// After a relation of a `FROM` list, where an alias may be typed.
const FROM_AFTER_RELATION: &[&str] = &[
    "AS",
    "WHERE",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "NATURAL",
    "OUTER",
    "GROUP",
    "HAVING",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "WINDOW",
];

/// After the relation of a `JOIN`, where an alias may be typed.
const JOIN_AFTER_RELATION: &[&str] = &["AS", "ON", "USING"];

/// After the relation of a `JOIN` and its alias.
pub(crate) const JOIN_TAIL: &[&str] = &["ON", "USING"];

/// After `LEFT`, `INNER`, `CROSS`, ...
pub(crate) const JOIN_CONT: &[&str] = &["JOIN", "OUTER", "APPLY"];

/// After the target of `UPDATE`.
const UPDATE_AFTER_TARGET: &[&str] = &["SET", "AS", "JOIN", "INNER", "LEFT"];

/// After the target of `DELETE FROM`.
const DELETE_AFTER_TARGET: &[&str] = &[
    "WHERE",
    "AS",
    "USING",
    "FROM",
    "RETURNING",
    "OUTPUT",
    "ORDER",
    "LIMIT",
];

/// After the target of `MERGE INTO`.
const MERGE_AFTER_TARGET: &[&str] = &["AS", "USING"];

/// After the table of `INSERT INTO`.
pub(crate) const INSERT_AFTER_TARGET: &[&str] =
    &["VALUES", "SELECT", "DEFAULT", "SET", "WITH", "OUTPUT", "AS"];

/// After the column list of `INSERT INTO t (...)`.
pub(crate) const INSERT_SOURCE: &[&str] = &[
    "VALUES",
    "SELECT",
    "DEFAULT",
    "WITH",
    "OUTPUT",
    "OVERRIDING",
];

/// After the rows of an `INSERT`.
pub(crate) const INSERT_TAIL: &[&str] = &["ON", "RETURNING"];

/// The tail clauses of `UPDATE` / `DELETE`.
pub(crate) const DML_TAIL: &[&str] = &["WHERE", "FROM", "RETURNING", "OUTPUT", "ORDER", "LIMIT"];

/// The tail clauses of a `SELECT` after its `FROM` list.
pub(crate) const SELECT_TAIL: &[&str] = &[
    "WHERE",
    "GROUP",
    "HAVING",
    "WINDOW",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "FETCH",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "FOR",
];

/// After `CREATE`.
pub(crate) const CREATE_OBJECTS: &[&str] = &[
    "TABLE",
    "VIEW",
    "INDEX",
    "UNIQUE",
    "FUNCTION",
    "PROCEDURE",
    "TRIGGER",
    "SCHEMA",
    "DATABASE",
    "SEQUENCE",
    "TYPE",
    "MATERIALIZED",
    "TEMPORARY",
    "OR",
    "EXTENSION",
    "USER",
    "ROLE",
];

/// After `DROP`.
pub(crate) const DROP_OBJECTS: &[&str] = &[
    "TABLE",
    "VIEW",
    "INDEX",
    "FUNCTION",
    "PROCEDURE",
    "TRIGGER",
    "SCHEMA",
    "DATABASE",
    "SEQUENCE",
    "TYPE",
    "MATERIALIZED",
];

/// After `ALTER`.
pub(crate) const ALTER_OBJECTS: &[&str] = &[
    "TABLE",
    "VIEW",
    "INDEX",
    "FUNCTION",
    "PROCEDURE",
    "SCHEMA",
    "DATABASE",
    "SEQUENCE",
    "TYPE",
    "USER",
    "ROLE",
];

/// Actions of `ALTER TABLE t`.
pub(crate) const ALTER_TABLE_ACTIONS: &[&str] = &[
    "ADD", "DROP", "ALTER", "RENAME", "MODIFY", "CHANGE", "OWNER", "SET",
];

/// After `ALTER TABLE t ADD`.
pub(crate) const ADD_WHAT: &[&str] = &[
    "COLUMN",
    "CONSTRAINT",
    "PRIMARY",
    "FOREIGN",
    "UNIQUE",
    "CHECK",
    "INDEX",
];

/// After `ALTER TABLE t DROP`.
pub(crate) const DROP_WHAT: &[&str] = &["COLUMN", "CONSTRAINT", "INDEX", "PRIMARY", "FOREIGN"];

/// After a column's data type in a definition.
pub(crate) const COLUMN_CONSTRAINTS: &[&str] = &[
    "NOT",
    "NULL",
    "DEFAULT",
    "PRIMARY",
    "REFERENCES",
    "UNIQUE",
    "CHECK",
    "COLLATE",
    "GENERATED",
    "IDENTITY",
    "AUTO_INCREMENT",
    "CONSTRAINT",
];

/// The `MERGE` branches.
pub(crate) const MERGE_WHEN: &[&str] = &["WHEN", "OUTPUT"];

/// The actions of a `MERGE` branch.
pub(crate) const MERGE_ACTIONS: &[&str] = &["UPDATE", "DELETE", "INSERT", "DO"];

/// Keywords after an operand, by clause (`select_item`: in a select list, where an alias may
/// follow).
pub(crate) fn after_operand(clause: Clause, select_item: bool) -> &'static [&'static str] {
    if select_item {
        return SELECT_ITEM_AFTER;
    }
    match clause {
        Clause::Where => WHERE_AFTER,
        Clause::JoinOn => JOIN_ON_AFTER,
        Clause::Having => HAVING_AFTER,
        Clause::GroupBy => GROUP_AFTER,
        Clause::OrderBy => ORDER_AFTER,
        Clause::Limit => LIMIT_AFTER,
        _ => OPERATOR_AFTER,
    }
}

/// Keywords after a relation where an alias may be typed, by clause.
pub(crate) fn after_relation(clause: Clause) -> &'static [&'static str] {
    match clause {
        Clause::Join => JOIN_AFTER_RELATION,
        Clause::UpdateTarget => UPDATE_AFTER_TARGET,
        Clause::Target => DELETE_AFTER_TARGET,
        Clause::InsertTarget => MERGE_AFTER_TARGET,
        _ => FROM_AFTER_RELATION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SORTED: &[(&str, &[&str])] = &[
        ("STOP", STOP),
        ("STOP_FUNCTIONS", STOP_FUNCTIONS),
        ("OPERATORS", OPERATORS),
        ("OPERATOR_CONT", OPERATOR_CONT),
        ("PREFIX", PREFIX),
        ("VALUE_WORDS", VALUE_WORDS),
        ("TYPED_LITERALS", TYPED_LITERALS),
        ("INTERVAL_UNITS", INTERVAL_UNITS),
        ("TYPE_CONT", TYPE_CONT),
        ("STRONG", STRONG),
        ("FIELD_FIRST", FIELD_FIRST),
        ("SELECT_MODIFIERS", SELECT_MODIFIERS),
        ("JOIN_WORDS", JOIN_WORDS),
        ("JOIN_MODIFIERS", JOIN_MODIFIERS),
        ("LIMIT_WORDS", LIMIT_WORDS),
        ("ORDER_SUFFIX", ORDER_SUFFIX),
        ("FRAME", FRAME),
        ("CREATE_MODIFIERS", CREATE_MODIFIERS),
        ("TABLE_CONSTRAINTS", TABLE_CONSTRAINTS),
        ("PARAM_MODES", PARAM_MODES),
        ("BODY_START", BODY_START),
        ("MERGE_MATCH", MERGE_MATCH),
        ("DML_MODIFIERS", DML_MODIFIERS),
        ("TRANSACTION_NEXT", TRANSACTION_NEXT),
        ("END_SUFFIX", END_SUFFIX),
        ("SKIPPED_STATEMENTS", SKIPPED_STATEMENTS),
        ("CONDITIONS", CONDITIONS),
        ("TRANSACTIONS", TRANSACTIONS),
        ("SESSION", SESSION),
        ("ALTER_COLUMN_ACTIONS", ALTER_COLUMN_ACTIONS),
        ("DROP_NON_COLUMNS", DROP_NON_COLUMNS),
    ];

    #[test]
    fn searched_lists_are_sorted_upper_case() {
        for (name, list) in SORTED {
            for pair in list.windows(2) {
                if let [a, b] = pair {
                    assert!(a < b, "{name}: `{a}` must sort before `{b}`");
                }
            }
            for word in *list {
                assert_eq!(*word, word.to_ascii_uppercase(), "{name}: `{word}`");
                assert!(contains(list, word), "{name}: `{word}` not found");
                assert!(
                    contains(list, &word.to_ascii_lowercase()),
                    "{name}: `{word}`"
                );
            }
        }
        assert!(!contains(STOP, "orders"));
        assert!(!contains(STOP, ""));
    }

    #[test]
    fn completion_lists_are_upper_case() {
        let lists: &[&[&str]] = &[
            STATEMENT_STARTERS,
            EXPR_START,
            SELECT_START,
            QUERY_START,
            AFTER_WITH,
            AFTER_CTE_NAME,
            SELECT_ITEM_AFTER,
            WHERE_AFTER,
            JOIN_ON_AFTER,
            HAVING_AFTER,
            GROUP_AFTER,
            ORDER_AFTER,
            LIMIT_AFTER,
            OPERATOR_AFTER,
            FROM_TAIL,
            FROM_AFTER_RELATION,
            JOIN_AFTER_RELATION,
            JOIN_TAIL,
            JOIN_CONT,
            UPDATE_AFTER_TARGET,
            DELETE_AFTER_TARGET,
            MERGE_AFTER_TARGET,
            INSERT_AFTER_TARGET,
            INSERT_SOURCE,
            INSERT_TAIL,
            DML_TAIL,
            SELECT_TAIL,
            CREATE_OBJECTS,
            DROP_OBJECTS,
            ALTER_OBJECTS,
            ALTER_TABLE_ACTIONS,
            ADD_WHAT,
            DROP_WHAT,
            COLUMN_CONSTRAINTS,
            MERGE_WHEN,
            MERGE_ACTIONS,
        ];
        for list in lists {
            for word in *list {
                assert_eq!(*word, word.to_ascii_uppercase());
            }
        }
        assert_eq!(after_operand(Clause::Where, true), SELECT_ITEM_AFTER);
        assert_eq!(after_relation(Clause::From), FROM_AFTER_RELATION);
    }
}
