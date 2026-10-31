//! `redact_secrets`: one case per keyword and literal form, connection strings, and the
//! "nothing to redact" path.

use std::borrow::Cow;

use oxbase_sql::{REDACTED_LITERAL, SqlDialect, redact_secrets};
use pretty_assertions::assert_eq;

const PG: SqlDialect = SqlDialect::Postgres;
const MYSQL: SqlDialect = SqlDialect::MySql { mariadb: false };
const MARIADB: SqlDialect = SqlDialect::MySql { mariadb: true };
const SQLITE: SqlDialect = SqlDialect::Sqlite;
const TSQL: SqlDialect = SqlDialect::TSql;

#[track_caller]
fn redacts(dialect: SqlDialect, sql: &str, expected: &str) {
    let out = redact_secrets(dialect, sql);
    assert!(
        matches!(out, Cow::Owned(_)),
        "{dialect:?}: {sql:?} was not redacted"
    );
    assert_eq!(out, expected, "{dialect:?}: {sql:?}");
}

#[track_caller]
fn keeps(dialect: SqlDialect, sql: &str) {
    let out = redact_secrets(dialect, sql);
    assert!(
        matches!(out, Cow::Borrowed(s) if std::ptr::eq(s, sql)),
        "{dialect:?}: {sql:?} became {out:?}"
    );
}

#[test]
fn the_redacted_literal() {
    assert_eq!(REDACTED_LITERAL, "'***'");
}

#[test]
fn postgres_passwords() {
    redacts(
        PG,
        "CREATE ROLE app LOGIN PASSWORD 'hunter2'",
        "CREATE ROLE app LOGIN PASSWORD '***'",
    );
    redacts(
        PG,
        "ALTER USER app WITH ENCRYPTED PASSWORD 'md5abc' VALID UNTIL '2030-01-01'",
        "ALTER USER app WITH ENCRYPTED PASSWORD '***' VALID UNTIL '2030-01-01'",
    );
    redacts(
        PG,
        "ALTER ROLE app PASSWORD $$x'y$$",
        "ALTER ROLE app PASSWORD '***'",
    );
    redacts(
        PG,
        "ALTER ROLE app PASSWORD $t$x$t$;",
        "ALTER ROLE app PASSWORD '***';",
    );
    redacts(
        PG,
        r"ALTER ROLE app PASSWORD E'a\'b'",
        "ALTER ROLE app PASSWORD '***'",
    );
    redacts(
        PG,
        "alter role app password /* new */ 'x'",
        "alter role app password /* new */ '***'",
    );
    redacts(
        PG,
        "CREATE USER MAPPING FOR bob SERVER s OPTIONS (user 'bob', password 'pw')",
        "CREATE USER MAPPING FOR bob SERVER s OPTIONS (user 'bob', password '***')",
    );
    redacts(
        PG,
        "UPDATE users SET password = 'x' WHERE id = 1",
        "UPDATE users SET password = '***' WHERE id = 1",
    );
    redacts(
        PG,
        "CREATE SECRET s (TYPE s3, SECRET 'abc')",
        "CREATE SECRET s (TYPE s3, SECRET '***')",
    );
    // Several secrets in one script; multi-byte text around them.
    redacts(
        PG,
        "SELECT 'é'; ALTER ROLE r PASSWORD 'ü😀'; ALTER ROLE q PASSWORD 'z'",
        "SELECT 'é'; ALTER ROLE r PASSWORD '***'; ALTER ROLE q PASSWORD '***'",
    );
}

#[test]
fn mysql_passwords() {
    for dialect in [MYSQL, MARIADB] {
        redacts(
            dialect,
            "CREATE USER 'app'@'%' IDENTIFIED BY 'pw'",
            "CREATE USER 'app'@'%' IDENTIFIED BY '***'",
        );
        redacts(
            dialect,
            "ALTER USER app IDENTIFIED WITH mysql_native_password BY \"pw\"",
            "ALTER USER app IDENTIFIED WITH mysql_native_password BY '***'",
        );
        redacts(
            dialect,
            "CREATE USER app IDENTIFIED WITH 'caching_sha2_password' BY 'pw'",
            "CREATE USER app IDENTIFIED WITH 'caching_sha2_password' BY '***'",
        );
        redacts(
            dialect,
            "CREATE USER app IDENTIFIED WITH caching_sha2_password AS '$A$005$hash'",
            "CREATE USER app IDENTIFIED WITH caching_sha2_password AS '***'",
        );
        redacts(
            dialect,
            "CREATE USER app IDENTIFIED BY PASSWORD '*94BDCEBE'",
            "CREATE USER app IDENTIFIED BY PASSWORD '***'",
        );
        redacts(dialect, "SET PASSWORD = 'pw'", "SET PASSWORD = '***'");
        redacts(
            dialect,
            "SET PASSWORD = PASSWORD('pw')",
            "SET PASSWORD = PASSWORD('***')",
        );
        redacts(
            dialect,
            "SET PASSWORD FOR 'app'@'localhost' = 'pw'",
            "SET PASSWORD FOR 'app'@'localhost' = '***'",
        );
        redacts(
            dialect,
            "CHANGE MASTER TO MASTER_HOST='db', MASTER_USER='r', MASTER_PASSWORD='pw', MASTER_PORT=3306",
            "CHANGE MASTER TO MASTER_HOST='db', MASTER_USER='r', MASTER_PASSWORD='***', MASTER_PORT=3306",
        );
        redacts(
            dialect,
            "CHANGE REPLICATION SOURCE TO SOURCE_PASSWORD = 'pw'",
            "CHANGE REPLICATION SOURCE TO SOURCE_PASSWORD = '***'",
        );
        redacts(
            dialect,
            "ALTER USER app IDENTIFIED BY 'new' REPLACE 'old' RETAIN CURRENT PASSWORD",
            "ALTER USER app IDENTIFIED BY '***' REPLACE '***' RETAIN CURRENT PASSWORD",
        );
        redacts(
            dialect,
            "CREATE USER app IDENTIFIED BY _utf8mb4'pw'",
            "CREATE USER app IDENTIFIED BY _utf8mb4'***'",
        );
        redacts(
            dialect,
            r"create user app identified by 'p\'w'",
            "create user app identified by '***'",
        );
        redacts(
            dialect,
            "/*!80001 CREATE USER app IDENTIFIED BY 'pw' */",
            "/*!80001 CREATE USER app IDENTIFIED BY '***' */",
        );
        keeps(dialect, "# password 'x'\nSELECT 1");
        keeps(dialect, "SELECT REPLACE('a', 'b', 'c')");
        keeps(
            dialect,
            "CREATE TABLE t (id INT, KEY idx (id), PRIMARY KEY (id))",
        );
        keeps(
            dialect,
            "INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE a = 'x'",
        );
    }
}

#[test]
fn sqlite_keys() {
    redacts(
        SQLITE,
        "ATTACH 'secret.db' AS s KEY 'pw'",
        "ATTACH 'secret.db' AS s KEY '***'",
    );
    redacts(SQLITE, "PRAGMA key = 'pw'", "PRAGMA key = '***'");
    redacts(SQLITE, "PRAGMA key = \"x'2DD29CA8'\"", "PRAGMA key = '***'");
    redacts(SQLITE, "PRAGMA rekey = 'new'", "PRAGMA rekey = '***'");
    keeps(SQLITE, "CREATE TABLE t (a INT, PRIMARY KEY (\"a\"))");
}

#[test]
fn tsql_passwords() {
    redacts(
        TSQL,
        "ALTER LOGIN app WITH PASSWORD = N'pw'",
        "ALTER LOGIN app WITH PASSWORD = N'***'",
    );
}

#[test]
fn connection_strings_inside_literals() {
    for dialect in [PG, MYSQL, SQLITE, TSQL] {
        redacts(
            dialect,
            "SELECT dblink_connect('host=db password=s3cret dbname=app')",
            "SELECT dblink_connect('host=db password=*** dbname=app')",
        );
        redacts(
            dialect,
            "SELECT 'Server=x;Pwd=abc;Database=y'",
            "SELECT 'Server=x;Pwd=***;Database=y'",
        );
        redacts(
            dialect,
            "SELECT 'password = abc&x=1'",
            "SELECT 'password = ***&x=1'",
        );
        redacts(
            dialect,
            "SELECT 'host=x password=''a b'' port=5'",
            "SELECT 'host=x password=*** port=5'",
        );
        redacts(
            dialect,
            "SELECT 'PASSWORD=a', 'pwd=b'",
            "SELECT 'PASSWORD=***', 'pwd=***'",
        );
        keeps(dialect, "SELECT 'mypassword=x', 'password', 'pwd = '");
    }
    redacts(
        PG,
        "SELECT dblink('dbname=x password=y', $$SELECT 1$$)",
        "SELECT dblink('dbname=x password=***', $$SELECT 1$$)",
    );
}

#[test]
fn nothing_to_redact_borrows() {
    for dialect in [PG, MYSQL, MARIADB, SQLITE, TSQL] {
        keeps(dialect, "");
        keeps(dialect, "SELECT * FROM t WHERE name = 'x'");
        keeps(dialect, "SELECT 1 -- PASSWORD 'x'\n");
        keeps(dialect, "SELECT /* password */ 'x'");
        keeps(dialect, "SELECT password FROM users WHERE id = 'x'");
        keeps(dialect, "ALTER USER app PASSWORD EXPIRE");
        keeps(dialect, "SELECT 'é😀'");
    }
    keeps(PG, "SELECT \"password\" FROM t WHERE a = 'x'");
    // Error messages go through the same function.
    keeps(PG, "password authentication failed for user \"app\"");
}
