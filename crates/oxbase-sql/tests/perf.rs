//! Benchmark-style tests: the splitter is linear. The release bounds (`cargo test --release`)
//! are the contract's; CI runs a debug `cargo test`, where the bounds are about ten times
//! looser (unoptimised code is roughly that much slower) but still catch any super-linear
//! behaviour. Each timing is the best of a few runs, so a busy CI machine does not fail them.

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

/// Two statements: two `GO` batches (`;` never splits a T-SQL batch).
const TSQL_CHUNK: &str = "SELECT N'it''s; fine', [col;x]]y] FROM t; -- note; here\n\
    /* block; /* nested */ */ INSERT INTO t VALUES ('a;b', N'données 😀');\n\
    CREATE PROCEDURE p AS BEGIN TRY SELECT 1; END TRY BEGIN CATCH SELECT 2; END CATCH;\nGO\n\
    DECLARE @x int = 1; IF @x = 1 SELECT @x; ELSE SELECT 0;\ngo\n";

/// The shortest of `runs` timings of `f`.
fn best_of(runs: usize, mut f: impl FnMut()) -> Duration {
    (0..runs)
        .map(|_| {
            let started = Instant::now();
            f();
            started.elapsed()
        })
        .min()
        .unwrap_or_default()
}

#[track_caller]
fn timed_split(dialect: SqlDialect, chunk: &str, per_chunk: usize, bytes: usize, limit: Duration) {
    let (sql, repeats) = script(chunk, bytes);
    let mut count = 0;
    let elapsed = best_of(3, || count = split(dialect, &sql).len());
    assert_eq!(count, per_chunk * repeats);
    assert!(
        elapsed < limit,
        "{dialect:?}: splitting {} bytes took {elapsed:?} (limit {limit:?})",
        sql.len()
    );
}

#[test]
fn ten_megabytes_split_fast() {
    let limit = bound(Duration::from_secs(1), Duration::from_secs(10));
    timed_split(SqlDialect::Postgres, PG_CHUNK, 3, 10 << 20, limit);
    timed_split(
        SqlDialect::MySql { mariadb: false },
        MYSQL_CHUNK,
        3,
        10 << 20,
        limit,
    );
    timed_split(SqlDialect::TSql, TSQL_CHUNK, 2, 10 << 20, limit);
}

#[test]
fn one_megabyte_splits_in_well_under_100_ms() {
    let limit = bound(Duration::from_millis(100), Duration::from_secs(1));
    timed_split(SqlDialect::Postgres, PG_CHUNK, 3, 1 << 20, limit);
    timed_split(
        SqlDialect::Sqlite,
        "SELECT 'a;b', [c;d] FROM t; -- x\n",
        1,
        1 << 20,
        limit,
    );
    timed_split(SqlDialect::TSql, TSQL_CHUNK, 2, 1 << 20, limit);
}

#[test]
fn one_huge_statement_and_one_huge_literal() {
    let limit = bound(Duration::from_millis(500), Duration::from_secs(5));
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
    // Nested redaction over a 4 MB dollar-quoted body full of dynamic SQL.
    let body = format!(
        "DO $$ {} $$",
        "EXECUTE 'ALTER ROLE r PASSWORD ''x'''; ".repeat(100_000)
    );
    let redacted = redact_secrets(SqlDialect::Postgres, &body);
    assert!(redacted.contains("PASSWORD ''***''") && !redacted.contains("''x''"));
    // T-SQL: one 4 MB batch (no `GO` line), and redaction over a large script.
    let (sql, _) = script("DECLARE @v int = 1; SELECT @v; ", 4 << 20);
    assert_eq!(split(SqlDialect::TSql, &sql).len(), 1);
    let (sql, _) = script(
        "ALTER LOGIN l WITH PASSWORD = N'x'\nGO\nSELECT 'pwd=y';\n",
        2 << 20,
    );
    let redacted = redact_secrets(SqlDialect::TSql, &sql);
    assert!(redacted.contains("PASSWORD = N'***'") && !redacted.contains("N'x'"));
    let elapsed = started.elapsed();
    assert!(elapsed < limit * 3, "took {elapsed:?}");
}
