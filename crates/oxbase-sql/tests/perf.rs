//! Benchmark-style tests: the splitter is linear. The bounds are generous in debug builds
//! (`cargo test` on CI) and strict in release (`cargo test --release`).

use std::time::{Duration, Instant};

use oxbase_sql::{SqlDialect, redact_secrets, split};

/// Repeats `chunk` (holding `per_chunk` statements) until the script has `bytes` bytes.
fn script(chunk: &str, bytes: usize) -> (String, usize) {
    let repeats = bytes.div_ceil(chunk.len());
    (chunk.repeat(repeats), repeats)
}

fn bound(release: Duration, debug: Duration) -> Duration {
    if cfg!(debug_assertions) {
        debug
    } else {
        release
    }
}

const PG_CHUNK: &str = "SELECT a, 'it''s; fine', \"col;x\" FROM t WHERE b = $1; -- note; here\n\
    /* block; /* nested */ */ INSERT INTO t VALUES (E'\\';', $$x;y$$, 'données 😀');\n\
    DO $f$ BEGIN PERFORM 1; END $f$;\n";

const MYSQL_CHUNK: &str = "SELECT 'a\\';b', `c;d` FROM t; # c;\n\
    INSERT INTO t VALUES (\"x;y\", 'é');\n\
    CREATE PROCEDURE p() BEGIN IF 1 THEN SELECT 1; END IF; END;\n";

#[track_caller]
fn timed_split(dialect: SqlDialect, chunk: &str, per_chunk: usize, bytes: usize, limit: Duration) {
    let (sql, repeats) = script(chunk, bytes);
    let started = Instant::now();
    let statements = split(dialect, &sql);
    let elapsed = started.elapsed();
    assert_eq!(statements.len(), per_chunk * repeats);
    assert!(
        elapsed < limit,
        "{dialect:?}: splitting {} bytes took {elapsed:?} (limit {limit:?})",
        sql.len()
    );
}

#[test]
fn ten_megabytes_split_fast() {
    let limit = bound(Duration::from_secs(1), Duration::from_secs(30));
    timed_split(SqlDialect::Postgres, PG_CHUNK, 3, 10 << 20, limit);
    timed_split(
        SqlDialect::MySql { mariadb: false },
        MYSQL_CHUNK,
        3,
        10 << 20,
        limit,
    );
}

#[test]
fn one_megabyte_splits_in_well_under_100_ms() {
    let limit = bound(Duration::from_millis(100), Duration::from_secs(5));
    timed_split(SqlDialect::Postgres, PG_CHUNK, 3, 1 << 20, limit);
    timed_split(
        SqlDialect::Sqlite,
        "SELECT 'a;b', [c;d] FROM t; -- x\n",
        1,
        1 << 20,
        limit,
    );
}

#[test]
fn one_huge_statement_and_one_huge_literal() {
    let limit = bound(Duration::from_millis(500), Duration::from_secs(15));
    // No terminator at all: one 4 MB statement.
    let (sql, _) = script("x + ", 4 << 20);
    let started = Instant::now();
    assert_eq!(split(SqlDialect::Postgres, &sql).len(), 1);
    // An unterminated dollar quote and an unterminated comment.
    let body = format!("SELECT $tag$ {}", "$ta $tag $$ ".repeat(300_000));
    assert_eq!(split(SqlDialect::Postgres, &body).len(), 1);
    let comment = format!("SELECT 1; /* {}", "/* ".repeat(1 << 20));
    assert_eq!(split(SqlDialect::Postgres, &comment).len(), 1);
    // Redaction over a large script.
    let (sql, _) = script("ALTER ROLE r PASSWORD 'x'; SELECT 'password=y';\n", 2 << 20);
    assert!(redact_secrets(SqlDialect::Postgres, &sql).contains("PASSWORD '***'"));
    let elapsed = started.elapsed();
    assert!(elapsed < limit * 2, "took {elapsed:?}");
}
