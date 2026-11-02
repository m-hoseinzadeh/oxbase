//! SQL Server language data for code insight (M6, `docs/m6-contracts.md` §4.4): the Transact-SQL
//! reserved keywords, the non-reserved keywords common in scripts, the built-in string, math,
//! date and time, aggregate, ranking and analytic, JSON, conversion, logical and system
//! functions, and the system data types. Written for Oxbase from the SQL Server documentation;
//! summaries are our own. Functions added in SQL Server 2017 (14.0) and 2022 (16.0) carry that
//! version (`since`).

use oxbase_dialect::{FunctionInfo, FunctionKind, SqlLanguage, TypeInfo, TypeParams};
use oxbase_types::Engine;

use FunctionKind::{Aggregate as A, Scalar as S, Table as T, Window as W};
use TypeParams::{Length as L, None as N, Precision as P, PrecisionScale as PS};

/// The SQL Server language data (registered by [`super::register`]).
pub const LANGUAGE: SqlLanguage = SqlLanguage {
    engine: Some(Engine::MsSql),
    keywords: KEYWORDS,
    reserved: RESERVED,
    functions: FUNCTIONS,
    types: TYPES,
    ident_chars: "@#$",
    default_schema: Some("dbo"),
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
    "ABSOLUTE", "ACTION", "ADD", "AFTER", "ALGORITHM", "ALL", "ALLOW_PAGE_LOCKS",
    "ALLOW_ROW_LOCKS", "ALTER", "AND", "ANSI_NULLS", "ANSI_PADDING", "ANSI_WARNINGS", "ANY",
    "APPLY", "ARITHABORT", "AS", "ASC", "AT", "AUTHORIZATION", "AUTO", "BACKUP", "BEFORE", "BEGIN",
    "BETWEEN", "BINARY", "BREAK", "BROWSE", "BULK", "BY", "CACHE", "CALLED", "CALLER", "CASCADE",
    "CASE", "CATCH", "CHANGE_TRACKING", "CHECK", "CHECKPOINT", "CLOSE", "CLUSTERED", "COALESCE",
    "COLLATE", "COLUMN", "COLUMNSTORE", "COMMIT", "COMMITTED", "COMPUTE",
    "CONCAT_NULL_YIELDS_NULL", "CONSTRAINT", "CONTAINS", "CONTAINSTABLE", "CONTINUE", "CONTROL",
    "CONVERT", "CREATE", "CROSS", "CURRENT", "CURRENT_DATE", "CURRENT_TIME", "CURRENT_TIMESTAMP",
    "CURRENT_USER", "CURSOR", "CYCLE", "DATABASE", "DATA_COMPRESSION", "DATEFIRST", "DATEFORMAT",
    "DBCC", "DEADLOCK_PRIORITY", "DEALLOCATE", "DECLARE", "DEFAULT", "DELAY", "DELETE", "DENY",
    "DESC", "DISABLE", "DISK", "DISTINCT", "DISTRIBUTED", "DOUBLE", "DROP", "DUMP", "DYNAMIC",
    "ELEMENTS", "ELSE", "ENABLE", "ENCRYPTION", "END", "ERRLVL", "ESCAPE", "EXCEPT", "EXEC",
    "EXECUTE", "EXISTS", "EXIT", "EXPLICIT", "EXTERNAL", "FAST_FORWARD", "FETCH", "FILE",
    "FILEGROUP", "FILESTREAM", "FILLFACTOR", "FILTER", "FIRST", "FOLLOWING", "FOR", "FORCESEEK",
    "FOREIGN", "FORWARD_ONLY", "FREETEXT", "FREETEXTTABLE", "FROM", "FULL", "FUNCTION", "GLOBAL",
    "GOTO", "GRANT", "GROUP", "HASH", "HAVING", "HOLDLOCK", "IDENTITY", "IDENTITYCOL",
    "IDENTITY_INSERT", "IF", "IGNORE_DUP_KEY", "IMPLICIT_TRANSACTIONS", "IN", "INCLUDE",
    "INCREMENT", "INDEX", "INNER", "INSERT", "INSERTED", "INSTEAD", "INTERSECT", "INTO", "IS",
    "ISOLATION", "JOIN", "JSON", "KEEP", "KEY", "KEYSET", "KILL", "LANGUAGE", "LAST", "LEFT",
    "LEVEL", "LIKE", "LINENO", "LOAD", "LOCAL", "LOCK_TIMEOUT", "LOGIN", "LOOP", "MARK", "MATCHED",
    "MAX", "MAXDOP", "MAXVALUE", "MERGE", "MINVALUE", "NAME", "NATIONAL", "NEXT", "NO", "NOCHECK",
    "NOCOUNT", "NOLOCK", "NONCLUSTERED", "NONE", "NOT", "NOWAIT", "NULL", "NULLIF",
    "NUMERIC_ROUNDABORT", "OF", "OFF", "OFFSET", "OFFSETS", "ON", "ONLINE", "ONLY", "OPEN",
    "OPENDATASOURCE", "OPENQUERY", "OPENROWSET", "OPENXML", "OPTIMISTIC", "OPTIMIZE", "OPTION",
    "OR", "ORDER", "OUT", "OUTER", "OUTPUT", "OVER", "OWNER", "PAD_INDEX", "PAGE", "PAGLOCK",
    "PARAMETERIZATION", "PARTITION", "PASSWORD", "PATH", "PERCENT", "PERSISTED", "PIVOT", "PLAN",
    "PRECEDING", "PRECISION", "PRIMARY", "PRINT", "PRIOR", "PROC", "PROCEDURE", "PUBLIC",
    "QUOTED_IDENTIFIER", "RAISERROR", "RANGE", "RAW", "READ", "READCOMMITTED", "READONLY",
    "READPAST", "READTEXT", "READUNCOMMITTED", "READ_ONLY", "RECOMPILE", "RECONFIGURE",
    "REFERENCES", "RELATIVE", "REPEATABLE", "REPEATABLEREAD", "REPLICATION", "RESTORE", "RESTRICT",
    "RETURN", "RETURNS", "REVERT", "REVOKE", "RIGHT", "ROLE", "ROLLBACK", "ROOT", "ROW",
    "ROWCOUNT", "ROWGUIDCOL", "ROWLOCK", "ROWS", "RULE", "SAVE", "SCHEMA", "SCHEMABINDING",
    "SCROLL", "SCROLL_LOCKS", "SECURITYAUDIT", "SELECT", "SELF", "SEMANTICKEYPHRASETABLE",
    "SEMANTICSIMILARITYDETAILSTABLE", "SEMANTICSIMILARITYTABLE", "SEQUENCE", "SERIALIZABLE",
    "SERVER", "SESSION_USER", "SET", "SETUSER", "SHUTDOWN", "SNAPSHOT", "SOME", "SOURCE", "SPARSE",
    "STATIC", "STATISTICS", "STATISTICS_NORECOMPUTE", "SYSTEM_USER", "SYSTEM_VERSIONING", "TABLE",
    "TABLESAMPLE", "TABLOCK", "TABLOCKX", "TARGET", "TEXTSIZE", "THEN", "THROW", "TIES", "TIME",
    "TIMEOUT", "TO", "TOP", "TRAN", "TRANSACTION", "TRIGGER", "TRUNCATE", "TRY", "TRY_CONVERT",
    "TSEQUAL", "TYPE", "TYPE_WARNING", "UNBOUNDED", "UNCOMMITTED", "UNION", "UNIQUE", "UNPIVOT",
    "UPDATE", "UPDATETEXT", "UPDLOCK", "USE", "USER", "VALUE", "VALUES", "VARYING", "VIEW",
    "WAITFOR", "WHEN", "WHERE", "WHILE", "WITH", "WITHIN", "WORK", "WRITETEXT", "XACT_ABORT",
    "XLOCK", "XML", "ZONE",
];

/// Words that cannot be bare identifiers or aliases.
#[rustfmt::skip]
const RESERVED: &[&str] = &[
    "ADD", "ALL", "ALTER", "AND", "ANY", "AS", "ASC", "AUTHORIZATION", "BACKUP", "BEGIN",
    "BETWEEN", "BREAK", "BROWSE", "BULK", "BY", "CASCADE", "CASE", "CHECK", "CHECKPOINT", "CLOSE",
    "CLUSTERED", "COALESCE", "COLLATE", "COLUMN", "COMMIT", "COMPUTE", "CONSTRAINT", "CONTAINS",
    "CONTAINSTABLE", "CONTINUE", "CONVERT", "CREATE", "CROSS", "CURRENT", "CURRENT_DATE",
    "CURRENT_TIME", "CURRENT_TIMESTAMP", "CURRENT_USER", "CURSOR", "DATABASE", "DBCC",
    "DEALLOCATE", "DECLARE", "DEFAULT", "DELETE", "DENY", "DESC", "DISK", "DISTINCT",
    "DISTRIBUTED", "DOUBLE", "DROP", "DUMP", "ELSE", "END", "ERRLVL", "ESCAPE", "EXCEPT", "EXEC",
    "EXECUTE", "EXISTS", "EXIT", "EXTERNAL", "FETCH", "FILE", "FILLFACTOR", "FOR", "FOREIGN",
    "FREETEXT", "FREETEXTTABLE", "FROM", "FULL", "FUNCTION", "GOTO", "GRANT", "GROUP", "HAVING",
    "HOLDLOCK", "IDENTITY", "IDENTITYCOL", "IDENTITY_INSERT", "IF", "IN", "INDEX", "INNER",
    "INSERT", "INTERSECT", "INTO", "IS", "JOIN", "KEY", "KILL", "LEFT", "LIKE", "LINENO", "LOAD",
    "MERGE", "NATIONAL", "NOCHECK", "NONCLUSTERED", "NOT", "NULL", "NULLIF", "OF", "OFF",
    "OFFSETS", "ON", "OPEN", "OPENDATASOURCE", "OPENQUERY", "OPENROWSET", "OPENXML", "OPTION",
    "OR", "ORDER", "OUTER", "OVER", "PERCENT", "PIVOT", "PLAN", "PRECISION", "PRIMARY", "PRINT",
    "PROC", "PROCEDURE", "PUBLIC", "RAISERROR", "READ", "READTEXT", "RECONFIGURE", "REFERENCES",
    "REPLICATION", "RESTORE", "RESTRICT", "RETURN", "REVERT", "REVOKE", "RIGHT", "ROLLBACK",
    "ROWCOUNT", "ROWGUIDCOL", "RULE", "SAVE", "SCHEMA", "SECURITYAUDIT", "SELECT",
    "SEMANTICKEYPHRASETABLE", "SEMANTICSIMILARITYDETAILSTABLE", "SEMANTICSIMILARITYTABLE",
    "SESSION_USER", "SET", "SETUSER", "SHUTDOWN", "SOME", "STATISTICS", "SYSTEM_USER", "TABLE",
    "TABLESAMPLE", "TEXTSIZE", "THEN", "TO", "TOP", "TRAN", "TRANSACTION", "TRIGGER", "TRUNCATE",
    "TRY_CONVERT", "TSEQUAL", "UNION", "UNIQUE", "UNPIVOT", "UPDATE", "UPDATETEXT", "USE", "USER",
    "VALUES", "VARYING", "VIEW", "WAITFOR", "WHEN", "WHERE", "WHILE", "WITH", "WITHIN",
    "WRITETEXT",
];

/// Built-in functions, sorted by name ignoring case (one entry per overload).
#[rustfmt::skip]
const FUNCTIONS: &[FunctionInfo] = &[
    f("ABS", "numeric", "same as input", S, 1, Some(1), "Absolute value of a number."),
    f("ACOS", "float", "float", S, 1, Some(1), "Arc cosine, in radians."),
    f("APPROX_COUNT_DISTINCT", "expr", "bigint", A, 1, Some(1), "Approximate number of distinct non-null values."),
    fs(f("APPROX_PERCENTILE_CONT", "fraction", "same as input", A, 1, Some(1), "Approximate interpolated percentile (with WITHIN GROUP)."), (16, 0)),
    fs(f("APPROX_PERCENTILE_DISC", "fraction", "same as input", A, 1, Some(1), "Approximate percentile value from the set (with WITHIN GROUP)."), (16, 0)),
    f("APP_NAME", "", "nvarchar", S, 0, Some(0), "Application name of the current session."),
    f("ASCII", "string", "int", S, 1, Some(1), "Code of the first character of a string."),
    f("ASIN", "float", "float", S, 1, Some(1), "Arc sine, in radians."),
    f("ATAN", "float", "float", S, 1, Some(1), "Arc tangent, in radians."),
    f("ATN2", "y, x", "float", S, 2, Some(2), "Arc tangent of y/x using the signs of both to pick the quadrant."),
    f("AVG", "expr", "numeric", A, 1, Some(1), "Mean of the non-null values in a group."),
    f("BINARY_CHECKSUM", "expr, ... or *", "int", S, 1, None, "Binary checksum of a row or of the given expressions."),
    f("CAST", "expr AS type", "type", S, 1, Some(1), "Value converted to another data type."),
    f("CEILING", "numeric", "same as input", S, 1, Some(1), "Smallest integer not below the number."),
    f("CHAR", "code", "char(1)", S, 1, Some(1), "Character with the given ASCII code."),
    f("CHARINDEX", "search, string, start", "int", S, 2, Some(3), "1-based position of a substring, 0 if absent."),
    f("CHECKSUM", "expr, ... or *", "int", S, 1, None, "Hash value of a row or of the given expressions."),
    f("CHECKSUM_AGG", "expr", "int", A, 1, Some(1), "Checksum of the values in a group."),
    f("CHOOSE", "index, value, ...", "type of the values", S, 2, None, "The value at a 1-based index of the list."),
    f("COALESCE", "value, ...", "type of the arguments", S, 1, None, "First argument that is not null."),
    f("COLUMNPROPERTY", "id, column, property", "int", S, 3, Some(3), "A property of a column or parameter."),
    f("COL_LENGTH", "table, column", "smallint", S, 2, Some(2), "Defined length of a column, in bytes."),
    f("COL_NAME", "table_id, column_id", "sysname", S, 2, Some(2), "Name of a column from table and column ids."),
    f("COMPRESS", "expr", "varbinary(max)", S, 1, Some(1), "Value compressed with GZIP."),
    f("CONCAT", "value, value, ...", "string", S, 2, None, "Arguments joined into one string, nulls as empty."),
    fs(f("CONCAT_WS", "separator, value, value, ...", "string", S, 3, None, "Arguments joined with a separator, nulls skipped."), (14, 0)),
    f("CONTEXT_INFO", "", "varbinary(128)", S, 0, Some(0), "Context information set for the session."),
    f("CONVERT", "type, expr, style", "type", S, 2, Some(3), "Value converted to another data type, with an optional style."),
    f("COS", "float", "float", S, 1, Some(1), "Cosine of an angle in radians."),
    f("COT", "float", "float", S, 1, Some(1), "Cotangent of an angle in radians."),
    f("COUNT", "expr or *", "int", A, 1, Some(1), "Number of rows, or of non-null values, in a group."),
    f("COUNT_BIG", "expr or *", "bigint", A, 1, Some(1), "Number of rows, or of non-null values, in a group, as bigint."),
    f("CUME_DIST", "", "float", W, 0, Some(0), "Share of the partition's rows up to and including this row's peers."),
    f("CURRENT_TIMEZONE", "", "varchar", S, 0, Some(0), "Time zone name of the server or database."),
    f("CURRENT_TRANSACTION_ID", "", "bigint", S, 0, Some(0), "Id of the current transaction."),
    f("CURSOR_STATUS", "scope, name", "smallint", S, 2, Some(2), "Whether a cursor is open, empty or closed."),
    f("DATABASEPROPERTYEX", "database, property", "sql_variant", S, 2, Some(2), "A database property or option value."),
    f("DATALENGTH", "expr", "int", S, 1, Some(1), "Number of bytes used to store a value."),
    f("DATEADD", "datepart, number, date", "same as date", S, 3, Some(3), "Date with a number of date parts added."),
    f("DATEDIFF", "datepart, start, end", "int", S, 3, Some(3), "Number of date-part boundaries crossed between two dates."),
    f("DATEDIFF_BIG", "datepart, start, end", "bigint", S, 3, Some(3), "Number of date-part boundaries crossed, as bigint."),
    f("DATEFROMPARTS", "year, month, day", "date", S, 3, Some(3), "Date from its parts."),
    f("DATENAME", "datepart, date", "nvarchar", S, 2, Some(2), "Name of a part of a date (month name, weekday name)."),
    f("DATEPART", "datepart, date", "int", S, 2, Some(2), "Integer value of a part of a date."),
    f("DATETIME2FROMPARTS", "year, month, day, hour, minute, seconds, fractions, precision", "datetime2", S, 8, Some(8), "datetime2 value from its parts."),
    f("DATETIMEFROMPARTS", "year, month, day, hour, minute, seconds, milliseconds", "datetime", S, 7, Some(7), "datetime value from its parts."),
    f("DATETIMEOFFSETFROMPARTS", "year, month, day, hour, minute, seconds, fractions, hour_offset, minute_offset, precision", "datetimeoffset", S, 10, Some(10), "datetimeoffset value from its parts."),
    fs(f("DATETRUNC", "datepart, date", "same as date", S, 2, Some(2), "Date truncated to the given part."), (16, 0)),
    fs(f("DATE_BUCKET", "datepart, width, date, origin", "same as date", S, 3, Some(4), "Start of the fixed-width time bucket a date falls into."), (16, 0)),
    f("DAY", "date", "int", S, 1, Some(1), "Day of the month of a date."),
    f("DB_ID", "name", "smallint", S, 0, Some(1), "Id of a database (the current one by default)."),
    f("DB_NAME", "id", "nvarchar", S, 0, Some(1), "Name of a database (the current one by default)."),
    f("DECOMPRESS", "expr", "varbinary(max)", S, 1, Some(1), "GZIP-compressed value decompressed."),
    f("DEGREES", "numeric", "same as input", S, 1, Some(1), "Radians converted to degrees."),
    f("DENSE_RANK", "", "bigint", W, 0, Some(0), "Rank of the row in its partition, without gaps after ties."),
    f("DIFFERENCE", "string, string", "int", S, 2, Some(2), "Similarity of the SOUNDEX codes of two strings, 0 to 4."),
    f("EOMONTH", "date, months", "date", S, 1, Some(2), "Last day of the month of a date, optionally moved by months."),
    f("ERROR_LINE", "", "int", S, 0, Some(0), "Line number where the caught error occurred."),
    f("ERROR_MESSAGE", "", "nvarchar", S, 0, Some(0), "Message of the caught error."),
    f("ERROR_NUMBER", "", "int", S, 0, Some(0), "Number of the caught error."),
    f("ERROR_PROCEDURE", "", "nvarchar", S, 0, Some(0), "Routine where the caught error occurred."),
    f("ERROR_SEVERITY", "", "int", S, 0, Some(0), "Severity of the caught error."),
    f("ERROR_STATE", "", "int", S, 0, Some(0), "State number of the caught error."),
    f("EXP", "float", "float", S, 1, Some(1), "e raised to the given power."),
    f("FIRST_VALUE", "expr", "type of expr", W, 1, Some(1), "Value from the first row of the window frame."),
    f("FLOOR", "numeric", "same as input", S, 1, Some(1), "Largest integer not above the number."),
    f("FORMAT", "value, format, culture", "nvarchar", S, 2, Some(3), "Value formatted with a .NET format string."),
    f("FORMATMESSAGE", "message, value, ...", "nvarchar", S, 1, None, "Message built from a message id or text and values."),
    fs(f("GENERATE_SERIES", "start, stop, step", "table", T, 2, Some(3), "One row per number from start to stop."), (16, 0)),
    f("GETDATE", "", "datetime", S, 0, Some(0), "Current date and time of the server."),
    f("GETUTCDATE", "", "datetime", S, 0, Some(0), "Current UTC date and time."),
    fs(f("GREATEST", "value, ...", "type of the arguments", S, 1, None, "Largest of the arguments."), (16, 0)),
    f("GROUPING", "column", "tinyint", A, 1, Some(1), "1 when the column is aggregated in a ROLLUP, CUBE or GROUPING SETS row."),
    f("GROUPING_ID", "column, ...", "int", A, 1, None, "Bit mask of the aggregated grouping columns."),
    f("HASHBYTES", "algorithm, input", "varbinary", S, 2, Some(2), "Hash of the input with MD5, SHA1 or SHA2."),
    f("HOST_NAME", "", "nvarchar", S, 0, Some(0), "Workstation name of the client."),
    f("IDENT_CURRENT", "table", "numeric", S, 1, Some(1), "Last identity value generated for a table."),
    f("IDENT_INCR", "table", "numeric", S, 1, Some(1), "Identity increment of a table."),
    f("IDENT_SEED", "table", "numeric", S, 1, Some(1), "Identity seed of a table."),
    f("IIF", "condition, then, else", "type of the results", S, 3, Some(3), "The second argument when the condition is true, else the third."),
    f("ISDATE", "expr", "int", S, 1, Some(1), "1 when the value is a valid date or time."),
    f("ISJSON", "expr, type", "bit", S, 1, Some(2), "1 when the string is valid JSON."),
    f("ISNULL", "expr, replacement", "type of expr", S, 2, Some(2), "The replacement when the expression is null."),
    f("ISNUMERIC", "expr", "int", S, 1, Some(1), "1 when the value converts to a numeric type."),
    fs(f("JSON_ARRAY", "value, ...", "nvarchar", S, 0, None, "JSON array text built from the arguments."), (16, 0)),
    f("JSON_MODIFY", "json, path, value", "nvarchar", S, 3, Some(3), "JSON text with the value at a path changed."),
    fs(f("JSON_OBJECT", "key:value, ...", "nvarchar", S, 0, None, "JSON object text built from key and value pairs."), (16, 0)),
    fs(f("JSON_PATH_EXISTS", "json, path", "bit", S, 2, Some(2), "1 when the path exists in the JSON text."), (16, 0)),
    f("JSON_QUERY", "json, path", "nvarchar", S, 1, Some(2), "Object or array extracted from JSON text."),
    f("JSON_VALUE", "json, path", "nvarchar", S, 2, Some(2), "Scalar extracted from JSON text."),
    f("LAG", "expr, offset, default", "type of expr", W, 1, Some(3), "Value from an earlier row of the partition."),
    f("LAST_VALUE", "expr", "type of expr", W, 1, Some(1), "Value from the last row of the window frame."),
    f("LEAD", "expr, offset, default", "type of expr", W, 1, Some(3), "Value from a later row of the partition."),
    fs(f("LEAST", "value, ...", "type of the arguments", S, 1, None, "Smallest of the arguments."), (16, 0)),
    f("LEFT", "string, count", "string", S, 2, Some(2), "Leftmost characters of a string."),
    f("LEN", "string", "int", S, 1, Some(1), "Number of characters, without trailing spaces."),
    f("LOG", "float, base", "float", S, 1, Some(2), "Natural logarithm, or the logarithm to a base."),
    f("LOG10", "float", "float", S, 1, Some(1), "Base-10 logarithm."),
    f("LOWER", "string", "string", S, 1, Some(1), "The string in lower case."),
    f("LTRIM", "string, characters", "string", S, 1, Some(2), "The string without leading spaces (or the given characters)."),
    f("MAX", "expr", "type of expr", A, 1, Some(1), "Largest non-null value in a group."),
    f("MIN", "expr", "type of expr", A, 1, Some(1), "Smallest non-null value in a group."),
    f("MONTH", "date", "int", S, 1, Some(1), "Month of a date."),
    f("NCHAR", "code", "nchar(1)", S, 1, Some(1), "Unicode character with the given code."),
    f("NEWID", "", "uniqueidentifier", S, 0, Some(0), "New random GUID."),
    f("NEWSEQUENTIALID", "", "uniqueidentifier", S, 0, Some(0), "New GUID greater than earlier ones (column defaults only)."),
    f("NTILE", "n", "bigint", W, 1, Some(1), "Group number from 1 to n that the row falls into."),
    f("NULLIF", "a, b", "type of a", S, 2, Some(2), "Null when both arguments are equal, else the first."),
    f("OBJECTPROPERTY", "id, property", "int", S, 2, Some(2), "A property of a schema-scoped object."),
    f("OBJECT_DEFINITION", "object_id", "nvarchar(max)", S, 1, Some(1), "Source text of a module."),
    f("OBJECT_ID", "name, type", "int", S, 1, Some(2), "Id of a schema-scoped object."),
    f("OBJECT_NAME", "object_id, database_id", "sysname", S, 1, Some(2), "Name of an object from its id."),
    f("OBJECT_SCHEMA_NAME", "object_id, database_id", "sysname", S, 1, Some(2), "Schema name of an object from its id."),
    f("OPENJSON", "json, path", "table", T, 1, Some(2), "Rows and columns read from JSON text."),
    f("OPENQUERY", "server, query", "table", T, 2, Some(2), "Rows of a pass-through query on a linked server."),
    f("OPENROWSET", "provider, connection, query", "table", T, 1, None, "Rows read from a remote data source or file."),
    f("PARSE", "string AS type", "type", S, 1, Some(1), "String converted to a date, time or number with a culture."),
    f("PARSENAME", "name, part", "nchar", S, 2, Some(2), "One part of a multi-part object name."),
    f("PATINDEX", "pattern, string", "int", S, 2, Some(2), "1-based position of the first LIKE pattern match, 0 if absent."),
    f("PERCENTILE_CONT", "fraction", "float", W, 1, Some(1), "Interpolated percentile of the ordered values (with WITHIN GROUP)."),
    f("PERCENTILE_DISC", "fraction", "type of the values", W, 1, Some(1), "Percentile value taken from the set (with WITHIN GROUP)."),
    f("PERCENT_RANK", "", "float", W, 0, Some(0), "Relative rank of the row, from 0 to 1."),
    f("PI", "", "float", S, 0, Some(0), "The number pi."),
    f("POWER", "numeric, power", "same as input", S, 2, Some(2), "A number raised to a power."),
    f("QUOTENAME", "string, quote", "nvarchar", S, 1, Some(2), "Name wrapped in brackets or quotes so it is a valid identifier."),
    f("RADIANS", "numeric", "same as input", S, 1, Some(1), "Degrees converted to radians."),
    f("RAND", "seed", "float", S, 0, Some(1), "Pseudo-random number from 0 to 1."),
    f("RANK", "", "bigint", W, 0, Some(0), "Rank of the row in its partition, with gaps after ties."),
    f("REPLACE", "string, pattern, replacement", "string", S, 3, Some(3), "The string with every occurrence of a substring replaced."),
    f("REPLICATE", "string, count", "string", S, 2, Some(2), "The string repeated a number of times."),
    f("REVERSE", "string", "string", S, 1, Some(1), "The string with its characters in reverse order."),
    f("RIGHT", "string, count", "string", S, 2, Some(2), "Rightmost characters of a string."),
    f("ROUND", "numeric, length, function", "same as input", S, 2, Some(3), "Number rounded (or truncated) to the given precision."),
    f("ROWCOUNT_BIG", "", "bigint", S, 0, Some(0), "Rows affected by the last statement, as bigint."),
    f("ROW_NUMBER", "", "bigint", W, 0, Some(0), "Position of the row in its partition, from 1."),
    f("RTRIM", "string, characters", "string", S, 1, Some(2), "The string without trailing spaces (or the given characters)."),
    f("SCHEMA_ID", "name", "int", S, 0, Some(1), "Id of a schema (the default one by default)."),
    f("SCHEMA_NAME", "id", "sysname", S, 0, Some(1), "Name of a schema (the default one by default)."),
    f("SCOPE_IDENTITY", "", "numeric", S, 0, Some(0), "Last identity value inserted in the current scope."),
    f("SERVERPROPERTY", "property", "sql_variant", S, 1, Some(1), "A property of the server instance."),
    f("SESSION_CONTEXT", "key", "sql_variant", S, 1, Some(1), "Value stored for a key in the session context."),
    f("SIGN", "numeric", "same as input", S, 1, Some(1), "-1, 0 or 1 for a negative, zero or positive number."),
    f("SIN", "float", "float", S, 1, Some(1), "Sine of an angle in radians."),
    f("SMALLDATETIMEFROMPARTS", "year, month, day, hour, minute", "smalldatetime", S, 5, Some(5), "smalldatetime value from its parts."),
    f("SOUNDEX", "string", "varchar(4)", S, 1, Some(1), "Four-character code of how a string sounds."),
    f("SPACE", "count", "varchar", S, 1, Some(1), "String of the given number of spaces."),
    f("SQRT", "float", "float", S, 1, Some(1), "Square root."),
    f("SQUARE", "float", "float", S, 1, Some(1), "Square of a number."),
    f("STDEV", "expr", "float", A, 1, Some(1), "Sample standard deviation of the values in a group."),
    f("STDEVP", "expr", "float", A, 1, Some(1), "Population standard deviation of the values in a group."),
    f("STR", "float, length, decimals", "varchar", S, 1, Some(3), "Number converted to right-aligned text."),
    fs(f("STRING_AGG", "expr, separator", "string", A, 2, Some(2), "Values of a group joined with a separator."), (14, 0)),
    f("STRING_ESCAPE", "text, type", "nvarchar", S, 2, Some(2), "Text with special characters escaped (JSON)."),
    f("STRING_SPLIT", "string, separator, enable_ordinal", "table", T, 2, Some(3), "One row per substring between separators."),
    f("STUFF", "string, start, length, insert", "string", S, 4, Some(4), "The string with a range of characters replaced by another string."),
    f("SUBSTRING", "string, start, length", "string", S, 3, Some(3), "Part of a string from a 1-based position."),
    f("SUM", "expr", "numeric", A, 1, Some(1), "Total of the non-null values in a group."),
    f("SUSER_NAME", "id", "nvarchar", S, 0, Some(1), "Login name from a login id (the current one by default)."),
    f("SUSER_SNAME", "sid", "nvarchar", S, 0, Some(1), "Login name from a security id (the current one by default)."),
    f("SWITCHOFFSET", "datetimeoffset, offset", "datetimeoffset", S, 2, Some(2), "The same instant expressed with another time zone offset."),
    f("SYSDATETIME", "", "datetime2", S, 0, Some(0), "Current date and time, with high precision."),
    f("SYSDATETIMEOFFSET", "", "datetimeoffset", S, 0, Some(0), "Current date and time with the server's time zone offset."),
    f("SYSUTCDATETIME", "", "datetime2", S, 0, Some(0), "Current UTC date and time, with high precision."),
    f("TAN", "float", "float", S, 1, Some(1), "Tangent of an angle in radians."),
    f("TIMEFROMPARTS", "hour, minute, seconds, fractions, precision", "time", S, 5, Some(5), "time value from its parts."),
    f("TODATETIMEOFFSET", "datetime, offset", "datetimeoffset", S, 2, Some(2), "A date and time combined with a time zone offset."),
    fs(f("TRANSLATE", "string, characters, translations", "string", S, 3, Some(3), "The string with characters replaced one for one."), (14, 0)),
    fs(f("TRIM", "[characters FROM] string", "string", S, 1, Some(1), "The string without spaces (or the given characters) at both ends."), (14, 0)),
    f("TRY_CAST", "expr AS type", "type", S, 1, Some(1), "Value converted to another type, or null when it cannot be."),
    f("TRY_CONVERT", "type, expr, style", "type", S, 2, Some(3), "Value converted to another type, or null when it cannot be."),
    f("TRY_PARSE", "string AS type", "type", S, 1, Some(1), "String parsed with a culture, or null when it cannot be."),
    f("TYPE_ID", "name", "int", S, 1, Some(1), "Id of a data type."),
    f("TYPE_NAME", "id", "sysname", S, 1, Some(1), "Name of a data type from its id."),
    f("UNICODE", "string", "int", S, 1, Some(1), "Code point of the first character of a string."),
    f("UPPER", "string", "string", S, 1, Some(1), "The string in upper case."),
    f("USER_ID", "name", "int", S, 0, Some(1), "Database user id (the current one by default)."),
    f("USER_NAME", "id", "nvarchar", S, 0, Some(1), "Database user name (the current one by default)."),
    f("VAR", "expr", "float", A, 1, Some(1), "Sample variance of the values in a group."),
    f("VARP", "expr", "float", A, 1, Some(1), "Population variance of the values in a group."),
    f("XACT_STATE", "", "smallint", S, 0, Some(0), "Whether the session has an active, committable transaction."),
    f("YEAR", "date", "int", S, 1, Some(1), "Year of a date."),
];

/// Built-in data types, sorted by name ignoring case.
#[rustfmt::skip]
const TYPES: &[TypeInfo] = &[
    ty("bigint", N, "64-bit integer."),
    ty("binary", L, "Fixed-length binary data."),
    ty("bit", N, "0, 1 or null."),
    ty("char", L, "Fixed-length non-Unicode string."),
    ty("cursor", N, "Reference to a cursor (variables and parameters only)."),
    ty("date", N, "Calendar date."),
    ty("datetime", N, "Date and time of day to about 3 ms."),
    ty("datetime2", P, "Date and time of day with up to 100 ns precision."),
    ty("datetimeoffset", P, "Date and time with a time zone offset."),
    ty("decimal", PS, "Exact number with the given precision and scale."),
    ty("float", P, "Floating point number (53-bit mantissa by default)."),
    ty("geography", N, "Round-earth spatial data."),
    ty("geometry", N, "Flat spatial data."),
    ty("hierarchyid", N, "Position in a tree hierarchy."),
    ty("image", N, "Variable-length binary data (deprecated; use varbinary(max))."),
    ty("int", N, "32-bit integer."),
    ty("money", N, "Currency amount with four decimal places."),
    ty("nchar", L, "Fixed-length Unicode string."),
    ty("ntext", N, "Long Unicode text (deprecated; use nvarchar(max))."),
    ty("numeric", PS, "Exact number with the given precision and scale."),
    ty("nvarchar", L, "Variable-length Unicode string, or max."),
    ty("real", N, "32-bit floating point number."),
    ty("rowversion", N, "Database-wide unique number that changes with every row update."),
    ty("smalldatetime", N, "Date and time of day to the minute."),
    ty("smallint", N, "16-bit integer."),
    ty("smallmoney", N, "Small currency amount with four decimal places."),
    ty("sql_variant", N, "Value of any of several base types."),
    ty("sysname", N, "nvarchar(128) used for object names."),
    ty("table", N, "Table variable or table-valued result."),
    ty("text", N, "Long non-Unicode text (deprecated; use varchar(max))."),
    ty("time", P, "Time of day with up to 100 ns precision."),
    ty("timestamp", N, "Synonym of rowversion (not a date or time)."),
    ty("tinyint", N, "Integer from 0 to 255."),
    ty("uniqueidentifier", N, "16-byte GUID."),
    ty("varbinary", L, "Variable-length binary data, or max."),
    ty("varchar", L, "Variable-length non-Unicode string, or max."),
    ty("xml", N, "XML document or fragment."),
];

#[cfg(test)]
mod tests {
    use oxbase_dialect::DialectRegistry;

    use super::*;

    #[test]
    fn the_data_is_well_formed_and_registered() {
        assert_eq!(LANGUAGE.validate(), Vec::<String>::new());
        assert!(
            LANGUAGE.functions.len() >= 150,
            "{}",
            LANGUAGE.functions.len()
        );
        assert!(LANGUAGE.is_reserved("select"));
        assert!(LANGUAGE.is_reserved("tran"));
        assert!(LANGUAGE.is_keyword("nocount"));
        assert!(!LANGUAGE.is_reserved("output"));
        assert_eq!(LANGUAGE.function("getdate").len(), 1);
        assert!(LANGUAGE.type_info("NVARCHAR").is_some());
        let mut reg = DialectRegistry::new();
        super::super::register(&mut reg);
        assert!(reg.has_language(Engine::MsSql));
        let problems: Vec<String> = reg
            .missing_insight_roles(Engine::MsSql)
            .into_iter()
            .filter(|p| p.contains("SqlLanguage"))
            .collect();
        assert_eq!(problems, Vec::<String>::new());
    }
}
