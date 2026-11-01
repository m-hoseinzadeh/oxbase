//! Table-driven tests for `split` / `statement_at`: one case per rule of the crate docs.

use oxbase_sql::{
    Position, SqlDialect, Statement, StatementKind, position_at, split, statement_at,
};
use pretty_assertions::assert_eq;

const PG: SqlDialect = SqlDialect::Postgres;
const MYSQL: SqlDialect = SqlDialect::MySql { mariadb: false };
const MARIADB: SqlDialect = SqlDialect::MySql { mariadb: true };
const SQLITE: SqlDialect = SqlDialect::Sqlite;
const TSQL: SqlDialect = SqlDialect::TSql;

/// Splits and checks the structural invariants every result must hold.
#[track_caller]
fn checked_split(dialect: SqlDialect, sql: &str) -> Vec<Statement> {
    let statements = split(dialect, sql);
    let mut previous_segment_end = 0;
    for s in &statements {
        assert!(s.range.start >= previous_segment_end, "overlap: {s:?}");
        assert!(s.range.start < s.range.end, "empty statement: {s:?}");
        assert!(s.range.end <= s.segment_end, "{s:?}");
        assert!(s.segment_end <= sql.len(), "{s:?}");
        assert_eq!(sql.get(s.range.clone()), Some(s.text.as_str()));
        assert_eq!(s.start, position_at(sql, s.range.start));
        assert_eq!(s.end, position_at(sql, s.range.end));
        previous_segment_end = s.segment_end;
    }
    statements
}

#[track_caller]
fn case(dialect: SqlDialect, sql: &str, expected: &[&str]) {
    let texts: Vec<String> = checked_split(dialect, sql)
        .into_iter()
        .map(|s| s.text)
        .collect();
    assert_eq!(texts, expected, "dialect {dialect:?}, script {sql:?}");
}

#[track_caller]
fn case_all(dialects: &[SqlDialect], sql: &str, expected: &[&str]) {
    for &dialect in dialects {
        case(dialect, sql, expected);
    }
}

const SEMICOLON_DIALECTS: [SqlDialect; 4] = [PG, MYSQL, MARIADB, SQLITE];

#[test]
fn basic_statements() {
    let d = &SEMICOLON_DIALECTS;
    case_all(d, "SELECT 1; SELECT 2", &["SELECT 1", "SELECT 2"]);
    case_all(d, "SELECT 1;\n\n  SELECT 2;\n", &["SELECT 1", "SELECT 2"]);
    case_all(d, "SELECT 1", &["SELECT 1"]);
    case_all(d, "  SELECT\n  1  \n", &["SELECT\n  1"]);
    case_all(d, "", &[]);
    case_all(d, " \n\t\r\n", &[]);
}

#[test]
fn quotes_hide_semicolons() {
    let d = &SEMICOLON_DIALECTS;
    case_all(
        d,
        "SELECT 'a;b'; SELECT 'it''s;'",
        &["SELECT 'a;b'", "SELECT 'it''s;'"],
    );
    // An identifier in PostgreSQL and SQLite, a string in MySQL: never a terminator.
    case_all(
        d,
        r#"SELECT "a;""b" FROM t; SELECT 2"#,
        &[r#"SELECT "a;""b" FROM t"#, "SELECT 2"],
    );
}

#[test]
fn comments_lead_trail_and_only() {
    let d = &SEMICOLON_DIALECTS;
    case_all(
        d,
        "-- lead;\n/* lead; */ SELECT 1 /* in; */ + 1 -- tail;\n; SELECT 2",
        &["SELECT 1 /* in; */ + 1 -- tail;", "SELECT 2"],
    );
    case_all(d, ";;  ;\n-- only a comment;\n/* and; this */", &[]);
    case_all(d, "SELECT 1; -- trailing; comment\n", &["SELECT 1"]);
    case_all(d, "SELECT 1 -- note", &["SELECT 1 -- note"]);
    case_all(d, "SELECT 1 -- note \r\n", &["SELECT 1 -- note"]);
}

#[test]
fn unterminated_constructs_run_to_the_end() {
    let d = &SEMICOLON_DIALECTS;
    case_all(
        d,
        "SELECT 'unterminated; SELECT 2",
        &["SELECT 'unterminated; SELECT 2"],
    );
    case_all(d, "SELECT 1; /* unterminated; SELECT 2", &["SELECT 1"]);
    case_all(
        d,
        "SELECT 1 /* unterminated; SELECT 2\n",
        &["SELECT 1 /* unterminated; SELECT 2\n"],
    );
    case(PG, "SELECT $$ x; ", &["SELECT $$ x; "]);
    case(PG, "SELECT \"a;", &["SELECT \"a;"]);
    case(MYSQL, "SELECT `a;", &["SELECT `a;"]);
    case(SQLITE, "SELECT [a;", &["SELECT [a;"]);
    case(
        SQLITE,
        "CREATE TRIGGER t AFTER INSERT ON x BEGIN SELECT 1;",
        &["CREATE TRIGGER t AFTER INSERT ON x BEGIN SELECT 1;"],
    );
}

#[test]
fn postgres_rules() {
    case(
        PG,
        "SELECT $$a;b$$; SELECT $fn$ $$ ; $x$ $fn$; SELECT 3",
        &["SELECT $$a;b$$", "SELECT $fn$ $$ ; $x$ $fn$", "SELECT 3"],
    );
    case(
        PG,
        "SELECT $1; SELECT a$b$c; SELECT 2",
        &["SELECT $1", "SELECT a$b$c", "SELECT 2"],
    );
    case(
        PG,
        r"SELECT E'a\';b', e'\\'; SELECT 2",
        &[r"SELECT E'a\';b', e'\\'", "SELECT 2"],
    );
    // Standard strings: a backslash is an ordinary character.
    case(PG, r"SELECT 'a\'; SELECT 2", &[r"SELECT 'a\'", "SELECT 2"]);
    case(
        PG,
        "SELECT U&'d;', B'1;', X'f;'; SELECT 2",
        &["SELECT U&'d;', B'1;', X'f;'", "SELECT 2"],
    );
    case(
        PG,
        "SELECT 1 /* a /* b; */ c; */; SELECT 2",
        &["SELECT 1 /* a /* b; */ c; */", "SELECT 2"],
    );
    case(PG, "/* a /* b */ still; */ SELECT 1", &["SELECT 1"]);
    case(
        PG,
        "CREATE OR REPLACE FUNCTION f(a int) RETURNS int LANGUAGE sql\nBEGIN ATOMIC\n  SELECT a;\n  SELECT CASE WHEN a > 0 THEN 1 ELSE 2 END;\nEND;\nSELECT f(1);",
        &[
            "CREATE OR REPLACE FUNCTION f(a int) RETURNS int LANGUAGE sql\nBEGIN ATOMIC\n  SELECT a;\n  SELECT CASE WHEN a > 0 THEN 1 ELSE 2 END;\nEND",
            "SELECT f(1)",
        ],
    );
    case(
        PG,
        "CREATE PROCEDURE p() LANGUAGE sql BEGIN /* c */ atomic INSERT INTO t VALUES (1); end; CALL p()",
        &[
            "CREATE PROCEDURE p() LANGUAGE sql BEGIN /* c */ atomic INSERT INTO t VALUES (1); end",
            "CALL p()",
        ],
    );
    // `end` as a column name inside an SQL-standard body.
    let columns = "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT end FROM t; \
        SELECT t.end, end + 1 FROM t; END";
    case(PG, &format!("{columns}; SELECT 2"), &[columns, "SELECT 2"]);
    case(
        PG,
        "BEGIN; SELECT 1; END; COMMIT",
        &["BEGIN", "SELECT 1", "END", "COMMIT"],
    );
    case(
        PG,
        "DO $$\nBEGIN\n  PERFORM 1;\nEND\n$$;\nSELECT 2",
        &["DO $$\nBEGIN\n  PERFORM 1;\nEND\n$$", "SELECT 2"],
    );
    case(
        PG,
        "CREATE TABLE \"weird;name\" (id int); SELECT '$$;'; SELECT 3",
        &[
            "CREATE TABLE \"weird;name\" (id int)",
            "SELECT '$$;'",
            "SELECT 3",
        ],
    );
    // psql meta-commands are ordinary text; backticks and `#` mean nothing special.
    case(PG, "\\c other\nSELECT 1;", &["\\c other\nSELECT 1"]);
    case(PG, "SELECT `a;b`", &["SELECT `a", "b`"]);
    case(PG, "SELECT 1 # x; SELECT 2", &["SELECT 1 # x", "SELECT 2"]);
}

#[test]
fn mysql_quotes_and_comments() {
    let d = &[MYSQL, MARIADB];
    case_all(
        d,
        r#"SELECT 'a\';b', "c\";d", `e;``f`; SELECT 2"#,
        &[r#"SELECT 'a\';b', "c\";d", `e;``f`"#, "SELECT 2"],
    );
    case_all(
        d,
        "# lead;\nSELECT 1; # tail;\nSELECT 2 -- c;\n; SELECT 3",
        &["SELECT 1", "SELECT 2 -- c;", "SELECT 3"],
    );
    // `--` needs whitespace after it to start a comment.
    case_all(d, "SELECT 1 --1;\nSELECT 2", &["SELECT 1 --1", "SELECT 2"]);
    case_all(
        d,
        "SELECT 1 -- c;\nFROM dual; SELECT 2",
        &["SELECT 1 -- c;\nFROM dual", "SELECT 2"],
    );
    case_all(
        d,
        "SELECT 1 --\tc;\nFROM dual",
        &["SELECT 1 --\tc;\nFROM dual"],
    );
    // Block comments do not nest.
    case_all(
        d,
        "/* a /* b */ SELECT 1; SELECT 2 */",
        &["SELECT 1", "SELECT 2 */"],
    );
    // Executable comments and hints are content.
    case_all(
        d,
        "/*!40101 SET NAMES utf8mb4 */;\n/*+ hint; */ SELECT 1;\nSELECT /*+ BKA(t) */ * FROM t",
        &[
            "/*!40101 SET NAMES utf8mb4 */",
            "/*+ hint; */ SELECT 1",
            "SELECT /*+ BKA(t) */ * FROM t",
        ],
    );
    case_all(
        d,
        "/*M!100100 SELECT 1; */ ; SELECT 2",
        &["/*M!100100 SELECT 1; */", "SELECT 2"],
    );
    case_all(d, "SET @a := 1; SELECT @a", &["SET @a := 1", "SELECT @a"]);
}

#[test]
fn mysql_delimiter() {
    let d = &[MYSQL, MARIADB];
    case_all(
        d,
        "DELIMITER $$\nCREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND$$\nDELIMITER ;\nCALL p();\n",
        &[
            "CREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND",
            "CALL p()",
        ],
    );
    case_all(
        d,
        "delimiter //\nSELECT 1//\nSELECT 2 //\nDeLiMiTeR ;\nSELECT 3;",
        &["SELECT 1", "SELECT 2", "SELECT 3"],
    );
    case_all(
        d,
        "DELIMITER ;;\nSELECT 1;;\nSELECT 'a;;b';;\nDELIMITER ;\n",
        &["SELECT 1", "SELECT 'a;;b'"],
    );
    case_all(
        d,
        "-- setup\n  DELIMITER //\nSELECT 1; SELECT 2//",
        &["SELECT 1; SELECT 2"],
    );
    // Not at the start of a line, or inside a statement: ordinary text.
    case_all(d, "SELECT 1; DELIMITER $$", &["SELECT 1", "DELIMITER $$"]);
    case_all(
        d,
        "SELECT\nDELIMITER FROM t;\nSELECT 2",
        &["SELECT\nDELIMITER FROM t", "SELECT 2"],
    );
    // The terminator is part of the segment.
    let sql = "DELIMITER $$\nSELECT 1$$ SELECT 2";
    let statements = checked_split(MYSQL, sql);
    assert_eq!(statements.len(), 2);
    assert_eq!(
        (statements[0].range.clone(), statements[0].segment_end),
        (13..21, 23)
    );
    assert_eq!(
        (statements[1].range.clone(), statements[1].segment_end),
        (24..32, 32)
    );
}

#[test]
fn mysql_compound_bodies() {
    let d = &[MYSQL, MARIADB];
    let procedure = "CREATE PROCEDURE p() BEGIN DECLARE x INT DEFAULT 0; IF x > 0 THEN SELECT 1; \
        ELSEIF x < 0 THEN SELECT 2; ELSE SELECT 3; END IF; WHILE x < 3 DO SET x = x + 1; \
        END WHILE; lbl: LOOP LEAVE lbl; END LOOP lbl; REPEAT SET x = x - 1; UNTIL x = 0 \
        END REPEAT; CASE x WHEN 1 THEN SELECT 1; ELSE BEGIN END; END CASE; \
        SET x = CASE WHEN x THEN IF(x, 1, 2) ELSE 0 END; END";
    case_all(
        d,
        &format!("{procedure};\nSELECT 9"),
        &[procedure, "SELECT 9"],
    );
    let trigger = "CREATE DEFINER=`root`@`%` TRIGGER t BEFORE INSERT ON x FOR EACH ROW \
        BEGIN SET NEW.a = 1; END";
    case_all(d, &format!("{trigger}; SELECT 1"), &[trigger, "SELECT 1"]);
    let function = "CREATE DEFINER = 'admin'@'localhost' FUNCTION f(x INT) RETURNS INT \
        DETERMINISTIC\nBEGIN\n  DECLARE y INT;\n  SET y = IF(x > 0, x, -x);\n  RETURN y;\nEND";
    case_all(
        d,
        &format!("{function};\nSELECT f(1);"),
        &[function, "SELECT f(1)"],
    );
    let event = "create event e on schedule every 1 day do begin delete from t; \
        insert into log values (1); end";
    case_all(d, &format!("{event}; SELECT 1"), &[event, "SELECT 1"]);
    let trigger_if = "CREATE TRIGGER t BEFORE UPDATE ON x FOR EACH ROW IF NEW.a < 0 THEN \
        SET NEW.a = 0; END IF";
    case_all(
        d,
        &format!("{trigger_if}; SELECT 1"),
        &[trigger_if, "SELECT 1"],
    );
    // Bodies without BEGIN ... END are a single statement anyway.
    case_all(
        d,
        "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW SET NEW.a = 1; SELECT 1",
        &[
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW SET NEW.a = 1",
            "SELECT 1",
        ],
    );
    case_all(
        d,
        "CREATE PROCEDURE p() SELECT 1; SELECT 2",
        &["CREATE PROCEDURE p() SELECT 1", "SELECT 2"],
    );
    // `end` and `begin` are non-reserved: column names inside a body are not block keywords.
    let columns = "CREATE PROCEDURE p() BEGIN SELECT r.end INTO x FROM r; SELECT 2; \
        SELECT end, begin FROM r WHERE end = 1 AND r.begin < 2; SELECT end FROM r; \
        SELECT x, begin; UPDATE r SET end = NEW.begin; END";
    case_all(d, &format!("{columns};\nSELECT 3"), &[columns, "SELECT 3"]);
    case_all(
        d,
        "CREATE PROCEDURE p() SELECT begin; SELECT 2",
        &["CREATE PROCEDURE p() SELECT begin", "SELECT 2"],
    );
    case_all(
        d,
        "CREATE PROCEDURE p() SELECT x.end; SELECT 2",
        &["CREATE PROCEDURE p() SELECT x.end", "SELECT 2"],
    );
    // Transactions and IF/REPEAT functions are not blocks.
    case_all(
        d,
        "BEGIN; SELECT 1; COMMIT;",
        &["BEGIN", "SELECT 1", "COMMIT"],
    );
    case_all(
        d,
        "DROP TABLE IF EXISTS t; CREATE TABLE IF NOT EXISTS t (a INT); SELECT IF(1, 2, 3), REPEAT('a', 2);",
        &[
            "DROP TABLE IF EXISTS t",
            "CREATE TABLE IF NOT EXISTS t (a INT)",
            "SELECT IF(1, 2, 3), REPEAT('a', 2)",
        ],
    );
}

#[test]
fn mariadb_top_level_compound_statements() {
    let sql = "BEGIN NOT ATOMIC\n  SELECT 1;\n  SELECT 2;\nEND;\nSELECT 3";
    case(
        MARIADB,
        sql,
        &[
            "BEGIN NOT ATOMIC\n  SELECT 1;\n  SELECT 2;\nEND",
            "SELECT 3",
        ],
    );
    case(
        MYSQL,
        sql,
        &[
            "BEGIN NOT ATOMIC\n  SELECT 1",
            "SELECT 2",
            "END",
            "SELECT 3",
        ],
    );
    case(
        MARIADB,
        "IF 1 = 1 THEN SELECT 1; ELSE SELECT 2; END IF; SELECT 3",
        &["IF 1 = 1 THEN SELECT 1; ELSE SELECT 2; END IF", "SELECT 3"],
    );
    case(
        MARIADB,
        "FOR i IN 1..3 DO SELECT i; END FOR; SELECT 4",
        &["FOR i IN 1..3 DO SELECT i; END FOR", "SELECT 4"],
    );
}

#[test]
fn sqlite_rules() {
    let trigger = "CREATE TRIGGER t AFTER INSERT ON x BEGIN UPDATE y SET n = CASE WHEN new.a \
        THEN 1 ELSE 0 END; INSERT INTO z VALUES (1); END";
    case(
        SQLITE,
        &format!("{trigger}; SELECT 1"),
        &[trigger, "SELECT 1"],
    );
    let temp = "CREATE TEMP TRIGGER IF NOT EXISTS t BEFORE DELETE ON x\nBEGIN\n  SELECT \
        RAISE(ABORT, 'no;');\nEND";
    case(SQLITE, &format!("{temp};\nSELECT 1"), &[temp, "SELECT 1"]);
    let temporary = "create temporary trigger t instead of insert on v when new.a = (case \
        when 1 then 1 end) begin select 1; end";
    case(SQLITE, &format!("{temporary};"), &[temporary]);
    case(
        SQLITE,
        "BEGIN TRANSACTION; SELECT 1; END TRANSACTION; BEGIN IMMEDIATE; COMMIT;",
        &[
            "BEGIN TRANSACTION",
            "SELECT 1",
            "END TRANSACTION",
            "BEGIN IMMEDIATE",
            "COMMIT",
        ],
    );
    case(
        SQLITE,
        "SELECT [a;b], `c;d`, \"e;f\" FROM t; SELECT 2",
        &["SELECT [a;b], `c;d`, \"e;f\" FROM t", "SELECT 2"],
    );
    case(
        SQLITE,
        r"SELECT 'a\'; SELECT 2",
        &[r"SELECT 'a\'", "SELECT 2"],
    );
    case(
        SQLITE,
        "/* a /* b */ SELECT 1; SELECT 2",
        &["SELECT 1", "SELECT 2"],
    );
    case(
        SQLITE,
        "SELECT 1 # x; SELECT 2",
        &["SELECT 1 # x", "SELECT 2"],
    );
    case(
        SQLITE,
        "CREATE TABLE t (begin INT, end INT); SELECT 2",
        &["CREATE TABLE t (begin INT, end INT)", "SELECT 2"],
    );
    // `end` / `begin` as column names in a trigger.
    let columns = "CREATE TRIGGER t AFTER INSERT ON x WHEN new.begin > 0 BEGIN UPDATE y SET \
        end = new.end, \"end\" = 1 WHERE end > 0; SELECT end FROM y; END";
    case(
        SQLITE,
        &format!("{columns}; SELECT 1"),
        &[columns, "SELECT 1"],
    );
}

#[test]
fn tsql_go_batches() {
    case(
        TSQL,
        "SELECT 1; SELECT 2\nGO\nSELECT 3\ngo 2 -- again\nSELECT 'x\nGO\ny'\n",
        &["SELECT 1; SELECT 2", "SELECT 3", "SELECT 'x\nGO\ny'"],
    );
    // `GO` with other text on its line is not a separator.
    case(TSQL, "SELECT 1 GO", &["SELECT 1 GO"]);
    case(TSQL, "SELECT 1\nGOTO x", &["SELECT 1\nGOTO x"]);
    case(
        TSQL,
        "SELECT 1\nGO x\nSELECT 2",
        &["SELECT 1\nGO x\nSELECT 2"],
    );
    case(
        TSQL,
        "SELECT 1\n/* c */ GO\nSELECT 2",
        &["SELECT 1\n/* c */ GO\nSELECT 2"],
    );
    // Indented, any case, CRLF, a count, a trailing comment.
    case(TSQL, "  GO  \nSELECT 1\r\nGO\r\n", &["SELECT 1"]);
    case(
        TSQL,
        "SELECT 1\n\tGo\nSELECT 2\ngO 10\nSELECT 3\nGO -- next\nSELECT 4\ngo--x\n",
        &["SELECT 1", "SELECT 2", "SELECT 3", "SELECT 4"],
    );
    // Not inside strings, identifiers or (nested) comments.
    case(TSQL, "SELECT [a\nGO\n]]b]", &["SELECT [a\nGO\n]]b]"]);
    case(TSQL, "SELECT \"a\nGO\nb\"", &["SELECT \"a\nGO\nb\""]);
    case(TSQL, "SELECT N'a\nGO\nb'", &["SELECT N'a\nGO\nb'"]);
    case(TSQL, "/* /* */\nGO\n */ SELECT 1", &["SELECT 1"]);
    case(
        TSQL,
        "SELECT 1 -- GO\nGO\n-- only a comment\nGO\n",
        &["SELECT 1 -- GO"],
    );
    // Only separators, comments and `;`: nothing.
    case(TSQL, "GO\nGO 2\n-- c\n/* d */\nGO", &[]);
    case(
        TSQL,
        ";;  ;\n-- only a comment;\n/* and; this */\nGO\n;",
        &[],
    );
    // A `GO` line ends a batch even inside an unclosed block.
    case(
        TSQL,
        "BEGIN\n  SELECT 1;\nGO\nSELECT 2",
        &["BEGIN\n  SELECT 1;", "SELECT 2"],
    );
}

#[test]
fn tsql_semicolons_never_split_a_batch() {
    // `docs/m4-contracts.md` §6 and §8.3: one batch, two result sets on the server.
    case(
        TSQL,
        "SELECT 1 AS a; SELECT 2 AS b",
        &["SELECT 1 AS a; SELECT 2 AS b"],
    );
    // The batch's trailing `;` stays (`MERGE` needs it); leading `;` and comments do not.
    case(
        TSQL,
        "-- lead;\n; /* lead; /* nested; */ */ ;SELECT 1 /* in; */ -- tail;\n;\nGO\nMERGE t USING s ON t.a = s.a WHEN MATCHED THEN DELETE;",
        &[
            "SELECT 1 /* in; */ -- tail;\n;",
            "MERGE t USING s ON t.a = s.a WHEN MATCHED THEN DELETE;",
        ],
    );
    // Variables, #temp tables, control flow and modules: the whole batch, as the server
    // must see it.
    for batch in [
        "DECLARE @x int = 1;\nSELECT @x;",
        "CREATE TABLE #t (a int); INSERT INTO #t VALUES (1); SELECT a FROM #t",
        "INSERT INTO t DEFAULT VALUES; SELECT SCOPE_IDENTITY(), @@ROWCOUNT, ROWCOUNT_BIG()",
        "IF NOT EXISTS (SELECT 1 FROM t) RETURN; DROP TABLE t;",
        "IF 1 = 1 SELECT 1;\nELSE SELECT 2;",
        "BEGIN TRY\n  SELECT 1/0;\nEND TRY\nBEGIN CATCH\n  SELECT ERROR_MESSAGE();\nEND CATCH;",
        "BEGIN TRANSACTION; UPDATE t SET a = 1; COMMIT TRANSACTION;",
        "SET ANSI_NULLS ON;\nSET QUOTED_IDENTIFIER ON;\nGO",
        "CREATE PROCEDURE dbo.p @a int = 1\nAS\nBEGIN\n  SET NOCOUNT ON;\n  SELECT @a;\nEND;",
        "create proc p as select 1; select 2",
        "CREATE OR ALTER FUNCTION dbo.f (@x int) RETURNS TABLE AS RETURN (SELECT @x AS x);",
        "CREATE TRIGGER trg ON t AFTER INSERT AS\nBEGIN\n  UPDATE t SET n = n + 1;\nEND",
        "ALTER VIEW v AS WITH c AS (SELECT 1 AS a) SELECT a FROM c;",
    ] {
        let expected = batch.strip_suffix("\nGO").unwrap_or(batch);
        case(
            TSQL,
            &format!("{batch}\nGO\nSELECT 9"),
            &[expected, "SELECT 9"],
        );
    }
}

#[test]
fn tsql_strings_identifiers_and_comments() {
    case(
        TSQL,
        "SELECT N'it''s; ok', 'x;y', n'z;'\nGO\nSELECT 2",
        &["SELECT N'it''s; ok', 'x;y', n'z;'", "SELECT 2"],
    );
    // A backslash is an ordinary character.
    case(
        TSQL,
        "SELECT 'a\\'\nGO\nSELECT 2",
        &["SELECT 'a\\'", "SELECT 2"],
    );
    case(
        TSQL,
        "SELECT [a;b]]c] FROM [x\nGO\ny]\nGO\nSELECT \"q;\"\"r\"",
        &["SELECT [a;b]]c] FROM [x\nGO\ny]", "SELECT \"q;\"\"r\""],
    );
    case(
        TSQL,
        "SELECT #t.a, @@VERSION, [begin], [end] FROM #t\nGO",
        &["SELECT #t.a, @@VERSION, [begin], [end] FROM #t"],
    );
    // Unterminated constructs run to the end, across `GO` lines.
    case(
        TSQL,
        "SELECT 'abc;\nGO\nSELECT 2",
        &["SELECT 'abc;\nGO\nSELECT 2"],
    );
    case(TSQL, "SELECT [abc;\nGO\n", &["SELECT [abc;\nGO\n"]);
    case(
        TSQL,
        "SELECT 1; /* a /* b */\nGO\n",
        &["SELECT 1; /* a /* b */\nGO\n"],
    );
}

#[test]
fn tsql_statement_kinds() {
    let kinds = |sql| {
        split(TSQL, sql)
            .into_iter()
            .map(|s| s.kind)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        kinds(
            "select 1\nGO\nWITH c AS (SELECT 1) SELECT * FROM c\nGO\nexec p\nGO\nEXECUTE p\nGO\nVALUES (1)\nGO\n(SELECT 1)"
        ),
        [StatementKind::Query; 6]
    );
    assert_eq!(
        kinds(
            "UPDATE t SET a = 1; SELECT @@ROWCOUNT\nGO\nDECLARE @x int; SELECT @x\nGO\nCREATE PROCEDURE p AS SELECT 1"
        ),
        [StatementKind::Other; 3]
    );
}

#[test]
fn tsql_ranges_positions_and_statement_at() {
    let sql = "SELECT N'é😀;';\nGO\nSELECT [日本;語];  SELECT 3\n";
    let s = checked_split(TSQL, sql);
    assert_eq!(s.len(), 2);
    assert_eq!(s[0].text, "SELECT N'é😀;';");
    // The batch owns its `GO` line.
    assert_eq!((s[0].range.clone(), s[0].segment_end), (0..18, 21));
    assert_eq!(s[1].start, Position { line: 3, column: 1 });
    assert_eq!(s[1].text, "SELECT [日本;語];  SELECT 3");
    assert_eq!(
        s[1].end,
        Position {
            line: 3,
            column: 25
        }
    );
    // "Run current statement" runs the whole `GO` batch around the cursor.
    let at = |offset| statement_at(TSQL, sql, offset).map(|s| s.text);
    assert_eq!(at(0).as_deref(), Some("SELECT N'é😀;';"));
    assert_eq!(at(19).as_deref(), Some("SELECT N'é😀;';"));
    assert_eq!(at(21).as_deref(), Some("SELECT N'é😀;';"));
    assert_eq!(at(22).as_deref(), Some("SELECT [日本;語];  SELECT 3"));
    assert_eq!(
        at(sql.len()).as_deref(),
        Some("SELECT [日本;語];  SELECT 3")
    );
    assert_eq!(statement_at(TSQL, "GO\n-- c\nGO", 4), None);
}

#[test]
fn ranges_and_positions_with_multibyte_text() {
    let sql = "SELECT 'é';\n  SELECT '😀' ;\r\nSELECT 3";
    let s = checked_split(PG, sql);
    assert_eq!(s.len(), 3);
    assert_eq!((s[0].range.clone(), s[0].segment_end), (0..11, 12));
    assert_eq!(s[0].start, Position { line: 1, column: 1 });
    assert_eq!(
        s[0].end,
        Position {
            line: 1,
            column: 11
        }
    );
    assert_eq!((s[1].range.clone(), s[1].segment_end), (15..28, 30));
    assert_eq!(s[1].text, "SELECT '😀'");
    assert_eq!(s[1].start, Position { line: 2, column: 3 });
    // '😀' is two UTF-16 units.
    assert_eq!(
        s[1].end,
        Position {
            line: 2,
            column: 14
        }
    );
    assert_eq!((s[2].range.clone(), s[2].segment_end), (32..40, 40));
    assert_eq!(s[2].start, Position { line: 3, column: 1 });
    assert_eq!(s[2].end, Position { line: 3, column: 9 });

    // Non-ASCII identifiers and text never confuse the lexer.
    case(
        PG,
        "SELECT ñandú, '日本;語' FROM données; SELECT 2",
        &["SELECT ñandú, '日本;語' FROM données", "SELECT 2"],
    );
    case(
        PG,
        "SELECT $ŧag$ ; $ŧag$; SELECT 2",
        &["SELECT $ŧag$ ; $ŧag$", "SELECT 2"],
    );
}

#[test]
fn segment_ends() {
    let s = checked_split(PG, "SELECT 1;  SELECT 2");
    assert_eq!((s[0].range.clone(), s[0].segment_end), (0..8, 9));
    assert_eq!((s[1].range.clone(), s[1].segment_end), (11..19, 19));
    let s = checked_split(PG, "SELECT 1 ;;");
    assert_eq!((s[0].range.clone(), s[0].segment_end), (0..8, 10));
}

#[test]
fn statement_at_boundaries() {
    let sql = "SELECT 1;\n\nSELECT 2;  -- tail\nSELECT 3";
    let at = |offset| statement_at(PG, sql, offset).map(|s| s.text);
    for offset in 0..=9 {
        assert_eq!(at(offset).as_deref(), Some("SELECT 1"), "{offset}");
    }
    // A blank line after a statement belongs to the next one.
    assert_eq!(at(10).as_deref(), Some("SELECT 2"));
    assert_eq!(at(20).as_deref(), Some("SELECT 2"));
    assert_eq!(at(21).as_deref(), Some("SELECT 3"));
    assert_eq!(at(25).as_deref(), Some("SELECT 3"));
    assert_eq!(at(38).as_deref(), Some("SELECT 3"));
    assert_eq!(at(1000).as_deref(), Some("SELECT 3"));
    assert_eq!(statement_at(PG, "", 0), None);
    assert_eq!(statement_at(PG, "-- only", 3), None);
    assert_eq!(statement_at(PG, ";;", 1), None);
    // An offset inside a multi-byte character.
    assert_eq!(
        statement_at(PG, "SELECT 'é'; SELECT 2", 9)
            .map(|s| s.text)
            .as_deref(),
        Some("SELECT 'é'")
    );
    // MySQL custom delimiter: the cursor right after `$$` still picks the statement.
    let sql = "DELIMITER $$\nSELECT 1$$\nSELECT 2$$";
    assert_eq!(
        statement_at(MYSQL, sql, 23).map(|s| s.text).as_deref(),
        Some("SELECT 1")
    );
    assert_eq!(
        statement_at(MYSQL, sql, 24).map(|s| s.text).as_deref(),
        Some("SELECT 2")
    );
}

#[test]
fn statement_kinds() {
    use StatementKind::{Other, Query};
    let kinds = |dialect, sql| -> Vec<StatementKind> {
        checked_split(dialect, sql)
            .into_iter()
            .map(|s| s.kind)
            .collect()
    };
    assert_eq!(
        kinds(
            PG,
            "SELECT 1; with x as (select 1) select * from x; (SELECT 1); VALUES (1); TABLE t; \
             EXPLAIN SELECT 1; INSERT INTO t VALUES (1); CALL p(); FETCH ALL FROM c; \
             /* c */ show all; E'x'"
        ),
        [
            Query, Query, Query, Query, Query, Query, Other, Query, Query, Query, Other
        ]
    );
    assert_eq!(
        kinds(
            MYSQL,
            "SHOW TABLES; DESCRIBE t; desc t; UPDATE t SET a = 1; /*!1 x */"
        ),
        [Query, Query, Query, Other, Other]
    );
    assert_eq!(
        kinds(SQLITE, "PRAGMA table_info(t); DELETE FROM t"),
        [Query, Other]
    );
    assert_eq!(
        kinds(
            TSQL,
            "SELECT 1\nGO\nwith c as (select 1 as a) select * from c\nGO\nEXEC sp_who\nGO\n\
             execute p\nGO\n(SELECT 1)\nGO\nUPDATE t SET a = 1\nGO\nSHOW x\nGO\nN'x'"
        ),
        [Query, Query, Query, Query, Query, Other, Other, Other]
    );
    // A batch's kind is its first keyword's.
    assert_eq!(
        kinds(
            TSQL,
            "DECLARE @x int = 1; SELECT @x\nGO\nSELECT @x = 1; SELECT @x\nGO\nUPDATE t SET a = 1; SELECT 1"
        ),
        [Other, Query, Other]
    );
}
