//! SQL text processing: statement splitter (M2), secret redaction for history (M2); later the
//! lexer for code insight, dialect AST (via `sqlparser`) and formatter.
//!
//! # M2 API
//!
//! - [`split`]: cut a script into [`Statement`]s with byte ranges and editor positions.
//! - [`statement_at`]: the statement under the cursor ("run current statement").
//! - [`redact_secrets`]: mask obvious password literals before SQL text is stored in history.
//! - Position helpers for the editor: [`position_at`] and [`byte_offset_from_utf16`].
//!
//! Owner of the bodies of [`split`] and [`redact_secrets`]: the **sql** stream
//! (`docs/m2-contracts.md`). Everything else here is final.
//!
//! # What a statement is
//!
//! - `range` is a byte range into the script. It starts at the first character of the
//!   statement and ends after its last character. It **excludes** leading whitespace, leading
//!   ordinary comments, trailing whitespace and the terminator (`;`, the current MySQL
//!   `DELIMITER`, or a T-SQL `GO` line). Comments *inside* the statement and trailing
//!   comments before the terminator on the same statement stay in. MySQL executable
//!   comments `/*! ... */` and optimizer hints `/*+ ... */` are statement content, never
//!   "leading comments".
//! - `text` is `script[range]`, unchanged.
//! - `segment_end` is the byte just past the statement's terminator, or `range.end` when it
//!   has none (the last statement of a script without a trailing `;`). [`statement_at`] uses it.
//! - A segment that holds only whitespace and comments (`;;`, a trailing comment) produces no
//!   statement. Client directives (MySQL `DELIMITER x`, T-SQL `GO`) produce no statement
//!   either.
//! - Statements come out in script order and never overlap.
//!
//! # Dialect rules
//!
//! The splitter is a lexer, not a parser: it only has to find the terminators that end
//! statements. Each dialect must honour these, so that no `;` inside them ends a statement:
//!
//! **All dialects**: `'...'` strings with `''` doubling; `"..."` quoted identifiers with `""`
//! doubling (MySQL: strings, see below); `-- ...` line comments; `/* ... */` block comments;
//! parenthesis depth is *not* used (a `;` never appears inside parentheses outside of the
//! bodies listed below).
//!
//! **PostgreSQL** ([`SqlDialect::Postgres`]):
//! - dollar quotes `$$ ... $$` and `$tag$ ... $tag$` (a tag is an identifier without `$`; `$1`
//!   is a parameter, not a quote); bodies of `CREATE FUNCTION` / `DO` are usually dollar quoted;
//! - `E'...'` / `e'...'` strings, in which a backslash escapes the next character (also `\'`);
//!   `U&'...'` and `B'...'`/`X'...'` are ordinary strings;
//! - block comments **nest**: `/* a /* b */ still comment */`;
//! - SQL-standard bodies: `CREATE [OR REPLACE] FUNCTION|PROCEDURE ... BEGIN ATOMIC ... END`
//!   is one statement (track `BEGIN ATOMIC` ... matching `END`, with `CASE ... END` nested
//!   inside counted);
//! - psql meta-commands (`\c`, `\i`, ...) are not supported: they are ordinary text.
//!
//! **MySQL / MariaDB** ([`SqlDialect::MySql`]):
//! - `'...'` and `"..."` are strings (unless `ANSI_QUOTES`, which the splitter does not track);
//!   in both a backslash escapes the next character, and the quote may be doubled;
//! - `` `...` `` quoted identifiers with ``` `` ``` doubling;
//! - `#` starts a line comment; `--` starts one only when followed by whitespace or a
//!   control character (`--1` is minus minus one);
//! - block comments do **not** nest; `/*! ... */` and `/*+ ... */` are content;
//! - `DELIMITER <token>` at the start of a line (case-insensitive) changes the terminator until
//!   the next `DELIMITER` line; the directive line itself is not a statement. `DELIMITER ;`
//!   restores the default;
//! - without a `DELIMITER` change, `CREATE [DEFINER=...] {PROCEDURE|FUNCTION|TRIGGER|EVENT}` whose
//!   body is a `BEGIN ... END` compound statement is one statement: track `BEGIN`/`END` nesting
//!   and the block forms `IF ... END IF`, `CASE ... END CASE`, `LOOP ... END LOOP`,
//!   `WHILE ... END WHILE`, `REPEAT ... END REPEAT` (best effort; a script that uses `DELIMITER`
//!   is always split correctly);
//! - [`SqlDialect::MySql`]'s `mariadb` flag: MariaDB also accepts `BEGIN NOT ATOMIC ... END` as a
//!   top-level compound statement (one statement), and Oracle-mode `/` is not supported.
//!
//! **SQLite** ([`SqlDialect::Sqlite`]):
//! - `'...'` strings; `"..."`, `` `...` `` and `[...]` quoted identifiers;
//! - `CREATE [TEMP|TEMPORARY] TRIGGER ... BEGIN ... END` is one statement (the body's `;`
//!   end the inner statements, not the trigger; `CASE ... END` inside is counted);
//! - `BEGIN [DEFERRED|IMMEDIATE|EXCLUSIVE] [TRANSACTION];` outside a trigger is an ordinary
//!   statement.
//!
//! **T-SQL** ([`SqlDialect::TSql`], M4):
//! - `'...'` and `N'...'` strings with `''` doubling (a backslash is an ordinary character);
//!   `[...]` identifiers with `]]` doubling; `"..."` identifiers; `--` line comments; block
//!   comments **nest**; `@` and `#` are word characters (`@var`, `#temp`, `@@ROWCOUNT`);
//! - **batches**: a line holding only `GO` (case-insensitive, optionally indented with spaces
//!   or tabs, optionally followed by a repeat count and a `--` comment) ends a batch. `GO`
//!   inside a string, an identifier or a comment, or with other text on its line (`GOTO x`,
//!   `SELECT 1 GO`), is not a separator. The `GO` line is not part of any statement, and the
//!   last statement of a batch owns it (its `segment_end` is the end of the `GO` line). The
//!   repeat count is accepted and ignored (the batch runs once);
//! - **statements within a batch**: `;` ends a statement, except inside a block
//!   (`BEGIN ... END`, `BEGIN TRY ... END TRY BEGIN CATCH ... END CATCH` as one block,
//!   `CASE ... END`) and except in a module definition: a statement starting with
//!   `CREATE [OR ALTER] {PROC|PROCEDURE|FUNCTION|TRIGGER|VIEW}`,
//!   `ALTER {PROC|PROCEDURE|FUNCTION|TRIGGER|VIEW}` or `CREATE {RULE|DEFAULT}` runs to the end
//!   of its batch. `BEGIN TRAN[SACTION]`, `BEGIN DISTRIBUTED|DIALOG|CONVERSATION ...` and
//!   `END CONVERSATION` are ordinary statements. A `;` followed by `ELSE` does not end the
//!   statement (`IF x SELECT 1; ELSE SELECT 2`);
//! - **batch-scoped state keeps the batch whole**: a batch that uses a local variable
//!   (`@name`), `DECLARE`, `SCOPE_IDENTITY`, `@@ROWCOUNT`, `@@ERROR`, `GOTO` or `RETURN`
//!   outside module bodies is one statement (from its first to its last statement, without
//!   the final `;`), because each statement runs as its own batch and these do not survive
//!   from one batch to the next.
//!
//! [`StatementKind`] is only a hint for the UI (whether the statement probably returns rows);
//! the query runner never relies on it.

use std::borrow::Cow;
use std::ops::Range;

use oxbase_types::Engine;

mod lex;
mod redact;
mod splitter;

/// The SQL dialect to split, chosen from the connection's engine with
/// [`SqlDialect::for_engine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqlDialect {
    Postgres,
    /// MySQL, or MariaDB when `mariadb` is set.
    MySql {
        mariadb: bool,
    },
    Sqlite,
    /// SQL Server batches (M4). See the crate docs.
    TSql,
}

impl SqlDialect {
    /// The dialect of a SQL engine; `None` for MongoDB and Redis.
    pub const fn for_engine(engine: Engine) -> Option<Self> {
        match engine {
            Engine::Postgres => Some(Self::Postgres),
            Engine::MySql => Some(Self::MySql { mariadb: false }),
            Engine::MariaDb => Some(Self::MySql { mariadb: true }),
            Engine::Sqlite => Some(Self::Sqlite),
            Engine::MsSql => Some(Self::TSql),
            Engine::MongoDb | Engine::Redis => None,
        }
    }
}

/// A position in the script as the editor counts it: 1-based `line`, and 1-based `column`
/// counted in **UTF-16 code units** (what Monaco and JavaScript strings use). Lines end at
/// `\n`; a `\r` before it counts as a character of the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

/// Whether a statement probably returns rows. A hint for the UI only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatementKind {
    /// Starts with `SELECT`, `WITH`, `VALUES`, `TABLE`, `SHOW`, `DESCRIBE`/`DESC`, `EXPLAIN`,
    /// `PRAGMA`, `CALL`, `FETCH` or `(` (first keyword, case-insensitive). T-SQL: `SELECT`,
    /// `WITH`, `EXEC`, `EXECUTE`, `VALUES` or `(`; a whole batch (batch-scoped state) is a
    /// query only when each of its statements is.
    Query,
    Other,
}

/// One statement of a script. See the crate docs for the exact range rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    /// Byte range of the statement text in the script (no leading comments, no terminator).
    pub range: Range<usize>,
    /// Byte just past the terminator, or `range.end` without one.
    pub segment_end: usize,
    /// Position of `range.start`.
    pub start: Position,
    /// Position of `range.end` (exclusive, like Monaco's `endColumn`).
    pub end: Position,
    /// `script[range]`.
    pub text: String,
    pub kind: StatementKind,
}

/// Splits `text` into statements following the dialect rules in the crate docs.
///
/// Never fails: unterminated strings, comments or bodies run to the end of the text and make
/// one last statement.
pub fn split(dialect: SqlDialect, text: &str) -> Vec<Statement> {
    splitter::split(dialect, text)
}

/// The statement for "run current statement" at `byte_offset` (see [`pick_statement`]).
pub fn statement_at(dialect: SqlDialect, text: &str, byte_offset: usize) -> Option<Statement> {
    let statements = split(dialect, text);
    pick_statement(&statements, byte_offset).cloned()
}

/// Picks the statement that "owns" `byte_offset` among `statements` (as returned by
/// [`split`], in order).
///
/// Statement `i` owns the offsets in `(segment_end[i-1], segment_end[i]]`; the first one owns
/// `[0, segment_end[0]]`. So a cursor right after `SELECT 1;` picks `SELECT 1`, while a cursor
/// on a blank line after it picks the next statement. An offset past the last statement's
/// segment picks the last statement. `None` only when there are no statements.
pub fn pick_statement(statements: &[Statement], byte_offset: usize) -> Option<&Statement> {
    statements
        .iter()
        .find(|s| byte_offset <= s.segment_end)
        .or_else(|| statements.last())
}

/// Masks obvious secrets in `sql` before it is stored (query history). Returns the input
/// unchanged (borrowed) when there is nothing to mask.
///
/// Rule (all dialects, case-insensitive keywords, outside comments):
///
/// 1. A string literal (any quote form of the dialect, including PostgreSQL dollar quotes)
///    that follows one of these keywords is replaced by [`REDACTED_LITERAL`]. Between the
///    keyword and the literal there may be whitespace, comments, `=`, `(` and the words
///    `BY`, `WITH <plugin> BY`, `AS`:
///    `PASSWORD`, `IDENTIFIED` (MySQL `IDENTIFIED BY '...'`, `IDENTIFIED WITH plugin BY '...'`,
///    `IDENTIFIED BY PASSWORD '...'`), `MASTER_PASSWORD`, `SOURCE_PASSWORD`, `SECRET`,
///    `KEY` (SQLite/SQLCipher `ATTACH ... KEY '...'`, `PRAGMA key = '...'`).
///    This covers `CREATE/ALTER USER|ROLE ... PASSWORD '...'`, `ENCRYPTED PASSWORD '...'`,
///    MySQL `SET PASSWORD = '...'`, `PASSWORD('...')`, `CHANGE MASTER TO MASTER_PASSWORD='...'`,
///    and PostgreSQL `OPTIONS (password '...')` of user mappings and foreign servers.
/// 2. Inside any string literal, a connection-string value after `password=` or `pwd=`
///    (case-insensitive) up to the next whitespace, `;`, `&` or the end of the literal is
///    replaced by `***` (`dblink_connect('host=x password=secret')`).
///
/// Everything else, including the structure of the statement, is kept, so history stays
/// searchable. This is a best-effort filter for the obvious cases, not a guarantee; the
/// history can also be turned off (`docs/m2-contracts.md`).
pub fn redact_secrets(dialect: SqlDialect, sql: &str) -> Cow<'_, str> {
    redact::redact(dialect, sql)
}

/// What [`redact_secrets`] puts in place of a secret string literal.
pub const REDACTED_LITERAL: &str = "'***'";

/// The editor [`Position`] of `byte_offset` in `text`. An offset inside a multi-byte character
/// counts as that character's start; offsets past the end clamp to the end.
pub fn position_at(text: &str, byte_offset: usize) -> Position {
    let end = floor_char_boundary(text, byte_offset);
    let before = &text[..end];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line = before.matches('\n').count() + 1;
    let column = before[line_start..].encode_utf16().count() + 1;
    Position {
        line: u32::try_from(line).unwrap_or(u32::MAX),
        column: u32::try_from(column).unwrap_or(u32::MAX),
    }
}

/// Converts an offset in UTF-16 code units (Monaco `model.getOffsetAt`, JavaScript string
/// index) into a byte offset in `text`. An offset in the middle of a surrogate pair maps to the
/// start of that character; offsets past the end clamp to `text.len()`.
pub fn byte_offset_from_utf16(text: &str, utf16_offset: usize) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        let next = units + ch.len_utf16();
        if next > utf16_offset {
            return byte;
        }
        units = next;
    }
    text.len()
}

/// The largest char boundary `<= index` (clamped to the text).
fn floor_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    (0..=index)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stmt(text: &str, range: Range<usize>, segment_end: usize) -> Statement {
        Statement {
            start: position_at(text, range.start),
            end: position_at(text, range.end),
            text: text[range.clone()].to_owned(),
            range,
            segment_end,
            kind: StatementKind::Query,
        }
    }

    #[test]
    fn dialect_per_engine() {
        assert_eq!(
            SqlDialect::for_engine(Engine::Postgres),
            Some(SqlDialect::Postgres)
        );
        assert_eq!(
            SqlDialect::for_engine(Engine::MariaDb),
            Some(SqlDialect::MySql { mariadb: true })
        );
        assert_eq!(
            SqlDialect::for_engine(Engine::MySql),
            Some(SqlDialect::MySql { mariadb: false })
        );
        assert_eq!(
            SqlDialect::for_engine(Engine::Sqlite),
            Some(SqlDialect::Sqlite)
        );
        assert_eq!(
            SqlDialect::for_engine(Engine::MsSql),
            Some(SqlDialect::TSql)
        );
        assert_eq!(SqlDialect::for_engine(Engine::Redis), None);
    }

    #[test]
    fn positions_count_utf16_columns() {
        let text = "ab\nxé😀z\r\nq";
        assert_eq!(position_at(text, 0), Position { line: 1, column: 1 });
        assert_eq!(position_at(text, 2), Position { line: 1, column: 3 });
        assert_eq!(position_at(text, 3), Position { line: 2, column: 1 });
        // 'x' (1 byte), 'é' (2 bytes), '😀' (4 bytes, 2 UTF-16 units).
        assert_eq!(position_at(text, 4), Position { line: 2, column: 2 });
        assert_eq!(position_at(text, 6), Position { line: 2, column: 3 });
        assert_eq!(position_at(text, 10), Position { line: 2, column: 5 });
        // Inside the emoji: its start.
        assert_eq!(position_at(text, 8), Position { line: 2, column: 3 });
        let q = text.len() - 1;
        assert_eq!(position_at(text, q), Position { line: 3, column: 1 });
        assert_eq!(position_at(text, 999), Position { line: 3, column: 2 });
    }

    #[test]
    fn utf16_offsets_to_bytes() {
        let text = "é😀a";
        assert_eq!(byte_offset_from_utf16(text, 0), 0);
        assert_eq!(byte_offset_from_utf16(text, 1), 2);
        // Middle of the surrogate pair: start of the emoji.
        assert_eq!(byte_offset_from_utf16(text, 2), 2);
        assert_eq!(byte_offset_from_utf16(text, 3), 6);
        assert_eq!(byte_offset_from_utf16(text, 4), 7);
        assert_eq!(byte_offset_from_utf16(text, 99), 7);
        assert_eq!(byte_offset_from_utf16("", 3), 0);
    }

    #[test]
    fn picks_the_statement_owning_the_offset() {
        let text = "SELECT 1;\n\nSELECT 2;  -- tail\nSELECT 3";
        let s1 = stmt(text, 0..8, 9);
        let s2 = stmt(text, 11..19, 20);
        let s3 = stmt(text, 30..38, 38);
        let all = [s1, s2, s3];
        assert_eq!(pick_statement(&all, 0).unwrap().text, "SELECT 1");
        // Right after the `;` of the first statement.
        assert_eq!(pick_statement(&all, 9).unwrap().text, "SELECT 1");
        // Blank line after it: the next statement.
        assert_eq!(pick_statement(&all, 10).unwrap().text, "SELECT 2");
        // In the trailing comment after statement 2: statement 3's segment.
        assert_eq!(pick_statement(&all, 25).unwrap().text, "SELECT 3");
        assert_eq!(pick_statement(&all, 38).unwrap().text, "SELECT 3");
        assert_eq!(pick_statement(&all, 500).unwrap().text, "SELECT 3");
        assert_eq!(pick_statement(&[], 0), None);
        assert_eq!(all[1].start, Position { line: 3, column: 1 });
        assert_eq!(all[1].end, Position { line: 3, column: 9 });
    }
}
