//! Robustness and latency of the tolerant parser (`docs/m6-contracts.md` §8.1, §9): every
//! prefix, every single-token deletion and random cuts of a corpus through `outline` and
//! `context_at` never panic and keep their ranges in the text; pathological inputs finish in
//! linear-ish time; a 1 MiB script and a 10 000-token statement stay within their budgets.
//!
//! The other insight functions join this test at integration (their bodies belong to other
//! streams); files in `tests/corpus/*.sql` are picked up when present.

use std::time::{Duration, Instant};

use oxbase_insight::{CursorContext, Dialect, Outline, context_at, outline};
use oxbase_sql::{SqlTokenKind, tokenize};
use oxbase_types::Engine;

const ENGINES: [Engine; 5] = [
    Engine::Postgres,
    Engine::MySql,
    Engine::MariaDb,
    Engine::Sqlite,
    Engine::MsSql,
];

/// Statements of every dialect (each runs through every dialect: none may panic anywhere).
const CORPUS: &[&str] = &[
    "SELECT o.id, count(*) AS n FROM sales.orders o JOIN customers AS c ON c.id = o.cid WHERE o.total > 1 GROUP BY o.id HAVING count(*) > 2 ORDER BY n DESC LIMIT 10 OFFSET 5",
    "WITH RECURSIVE r (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 10) SELECT * FROM r",
    "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS v (n, s), LATERAL (SELECT n) l, generate_series(1, 3) g",
    "SELECT CASE WHEN a > 1 THEN 'x' WHEN a IS NULL THEN NULL ELSE 'y' END, sum(b) OVER (PARTITION BY c ORDER BY d ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t",
    "SELECT a::text, CAST(b AS numeric(10, 2)), EXTRACT(YEAR FROM c), d IS DISTINCT FROM e, f NOT IN (SELECT g FROM h) FROM t WHERE x BETWEEN 1 AND 2 AND y LIKE 'a%' ESCAPE '!'",
    "SELECT \"Mixed Case\".\"Col\" FROM \"Mixed Case\" WHERE $1 = :name AND ? IS NOT NULL",
    "SELECT string_agg(a, ',' ORDER BY b) FILTER (WHERE c) WITHIN GROUP (ORDER BY d) FROM t",
    "INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y') ON CONFLICT (a) DO UPDATE SET b = excluded.b WHERE t.a > 0 RETURNING *",
    "INSERT INTO t SELECT * FROM u WHERE NOT EXISTS (SELECT 1 FROM t WHERE t.id = u.id)",
    "UPDATE t SET a = a + 1, b = DEFAULT FROM u WHERE u.id = t.id RETURNING t.a",
    "DELETE FROM t USING u WHERE t.id = u.id",
    "MERGE INTO t AS d USING s ON d.id = s.id WHEN MATCHED AND s.x = 1 THEN UPDATE SET v = s.v WHEN NOT MATCHED THEN INSERT (id, v) VALUES (s.id, s.v)",
    "CREATE TABLE IF NOT EXISTS s.t (id bigint PRIMARY KEY, name varchar(10) NOT NULL DEFAULT 'x', p int REFERENCES parent (id) ON DELETE CASCADE, CONSTRAINT c CHECK (id > 0), FOREIGN KEY (p) REFERENCES other (k)) WITH (fillfactor = 70)",
    "CREATE OR REPLACE VIEW v (x) AS SELECT a FROM t WITH CHECK OPTION",
    "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS i ON t USING btree (a, lower(b) DESC NULLS LAST) INCLUDE (c) WHERE d",
    "CREATE FUNCTION f(a int, OUT b text) RETURNS SETOF record LANGUAGE sql STABLE AS $$ SELECT a, b FROM t WHERE c = a $$",
    "CREATE FUNCTION g() RETURNS trigger AS $body$ BEGIN NEW.x := 1; RETURN NEW; END $body$ LANGUAGE plpgsql",
    "CREATE TRIGGER tr BEFORE UPDATE OF a ON t FOR EACH ROW EXECUTE FUNCTION g()",
    "DO $$ BEGIN PERFORM 1; END $$",
    "ALTER TABLE ONLY t ADD COLUMN c int NOT NULL, DROP COLUMN IF EXISTS d, ALTER COLUMN e TYPE text USING e::text, RENAME COLUMN f TO g",
    "DROP TABLE IF EXISTS a, s.b CASCADE",
    "DROP FUNCTION IF EXISTS f(int, text)",
    "COMMENT ON COLUMN t.a IS 'note'",
    "GRANT SELECT, INSERT ON TABLE t TO role_a",
    "EXPLAIN (ANALYZE, BUFFERS) SELECT 1",
    "SELECT `select`, `a b` FROM `my db`.`t` USE INDEX (i) WHERE b = _utf8'x' AND c = @v AND d = @@session.sql_mode # note",
    "UPDATE a JOIN b ON a.id = b.a_id SET a.x = b.y, a.z = 1 WHERE b.k = 2 ORDER BY a.id LIMIT 3",
    "DELETE a FROM orders a JOIN items i ON i.oid = a.id WHERE i.n = 0",
    "INSERT INTO t (a, b) VALUES (1, 2) ON DUPLICATE KEY UPDATE b = VALUES(b) + 1",
    "INSERT IGNORE INTO t SET a = 1, b = 'x'",
    "CREATE DEFINER = `root`@`localhost` PROCEDURE p(IN a INT, OUT b VARCHAR(10)) BEGIN DECLARE n INT DEFAULT 0; DECLARE CONTINUE HANDLER FOR NOT FOUND SET n = 1; lbl: LOOP SELECT x INTO n FROM t WHERE y = a; IF n > 0 THEN LEAVE lbl; END IF; END LOOP; END",
    "CREATE TABLE t (id INT AUTO_INCREMENT, KEY k (id), INDEX (id), name TEXT) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4",
    "SELECT /*! STRAIGHT_JOIN */ a FROM t1 STRAIGHT_JOIN t2 ON t1.id = t2.id FOR UPDATE",
    "SELECT a FROM t INDEXED BY i WHERE b GLOB 'x*' AND c REGEXP 'y'",
    "INSERT OR REPLACE INTO t (a) VALUES (1)",
    "CREATE TRIGGER tr AFTER INSERT ON t FOR EACH ROW WHEN NEW.a > 0 BEGIN UPDATE u SET n = n + 1; END",
    "PRAGMA table_info(t)",
    "ATTACH DATABASE 'x.db' AS other",
    "SELECT TOP (5) WITH TIES total = SUM(o.amount), [Customer Name] = c.name FROM dbo.orders AS o WITH (NOLOCK) CROSS APPLY dbo.f(o.id) AS x OUTER APPLY (SELECT 1 AS one) y JOIN [dbo].[customers] c ON c.id = o.cid ORDER BY 1 OFFSET 10 ROWS FETCH NEXT 5 ROWS ONLY OPTION (RECOMPILE)",
    "DECLARE @n int = 1, @t TABLE (a int, b nvarchar(10))\nIF @n > 0\nBEGIN\n  UPDATE t SET a = 1 OUTPUT inserted.a INTO @t WHERE b = @n\nEND\nELSE PRINT N'none'\nSELECT c FROM u",
    "CREATE OR ALTER PROCEDURE dbo.p @a int, @b varchar(10) = NULL OUTPUT WITH RECOMPILE AS BEGIN SET NOCOUNT ON; BEGIN TRY SELECT c FROM t WHERE d = @a END TRY BEGIN CATCH THROW; END CATCH END",
    "CREATE FUNCTION dbo.f(@a int) RETURNS TABLE AS RETURN (SELECT a FROM t WHERE b = @a)",
    "CREATE TRIGGER tr ON t AFTER INSERT, UPDATE AS BEGIN UPDATE t SET m = GETDATE() FROM t JOIN inserted i ON i.id = t.id END",
    "EXEC @rc = dbo.p @a = 1, @b = 'x' OUTPUT",
    "WITH c AS (SELECT a FROM t) UPDATE c SET a = 1",
    "SELECT DATEADD(day, 1, a), CONVERT(varchar(10), b, 120), TRY_CAST(c AS int) FROM t FOR XML PATH(''), ROOT('r')",
    "BEGIN TRANSACTION; SAVEPOINT s; ROLLBACK TO SAVEPOINT s; COMMIT",
    "USE shop",
    "SELECT 1 FROM t WHERE a = 'unterminated",
    "SELECT \"unterminated FROM t",
    "SELECT [unterminated FROM t",
    "SELECT /* unterminated comment",
    "SELECT $$ unterminated body",
    "SELECT ((((a + b) * c) FROM t WHERE (d",
    "SELECT ) FROM ( WHERE ) AND ,, ;; ON ON ON",
    "SELECT * FROM заказы з WHERE з.сумма > 1 AND 名前 = 'é'",
];

fn dialects() -> Vec<Dialect> {
    ENGINES
        .iter()
        .map(|e| Dialect::builtin(*e).expect("a SQL engine"))
        .collect()
}

/// The corpus files of the diagnose stream, when present.
fn corpus_files() -> Vec<String> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut texts = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "sql")
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            texts.push(text);
        }
    }
    texts
}

fn texts() -> Vec<String> {
    let mut all: Vec<String> = CORPUS.iter().map(|s| (*s).to_owned()).collect();
    all.extend(corpus_files());
    all
}

fn check_outline(o: &Outline, text: &str) {
    for r in &o.refs {
        assert_eq!(r.parts.len(), r.ranges.len(), "{text:?}");
        for range in &r.ranges {
            assert!(
                range.start <= range.end && range.end <= text.len(),
                "{text:?}"
            );
        }
        if let Some(q) = r.query {
            assert!(q < o.queries.len(), "{text:?}");
        }
    }
    for (i, q) in o.queries.iter().enumerate() {
        assert!(
            q.range.start <= q.range.end && q.range.end <= text.len(),
            "{text:?}"
        );
        if let Some(p) = q.parent {
            assert!(p < i, "parents come first: {text:?}");
        }
        for (_, range) in &q.clauses {
            assert!(range.end <= text.len(), "{text:?}");
        }
    }
}

fn check_context(ctx: &CursorContext, text: &str) {
    assert!(ctx.offset <= text.len(), "{text:?}");
    assert!(
        ctx.word.start <= ctx.offset && ctx.offset <= ctx.word.end && ctx.word.end <= text.len(),
        "word {:?} at {} in {text:?}",
        ctx.word,
        ctx.offset
    );
    assert!(text.is_char_boundary(ctx.word.start) && text.is_char_boundary(ctx.word.end));
    assert!(ctx.statement.end <= text.len(), "{text:?}");
    check_outline(&ctx.outline, text);
}

fn run(dialect: &Dialect, text: &str, offsets: &[usize]) {
    let o = outline(dialect, text, 0..text.len());
    check_outline(&o, text);
    for &offset in offsets {
        let ctx = context_at(dialect, text, offset);
        check_context(&ctx, text);
    }
}

/// A small deterministic generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        usize::try_from(self.next_u64() % u64::try_from(n).unwrap_or(u64::MAX)).unwrap_or(0)
    }
}

/// The char boundary at or before `i`.
fn floor(text: &str, i: usize) -> usize {
    let mut i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[test]
fn every_prefix_never_panics() {
    for dialect in dialects() {
        for text in texts() {
            for (end, _) in text
                .char_indices()
                .chain(std::iter::once((text.len(), ' ')))
            {
                let prefix = &text[..end];
                let o = outline(&dialect, prefix, 0..prefix.len());
                check_outline(&o, prefix);
                let ctx = context_at(&dialect, prefix, prefix.len());
                check_context(&ctx, prefix);
            }
        }
    }
}

#[test]
fn every_offset_of_the_corpus_never_panics() {
    for dialect in dialects() {
        for text in texts() {
            let offsets: Vec<usize> = (0..=text.len() + 2).collect();
            run(&dialect, &text, &offsets);
        }
    }
}

#[test]
fn every_single_token_deletion_never_panics() {
    for dialect in dialects() {
        for text in texts() {
            let tokens: Vec<_> = tokenize(dialect.sql, &text)
                .filter(|t| t.kind != SqlTokenKind::Space)
                .collect();
            for t in tokens {
                let cut = format!("{}{}", &text[..t.start], &text[t.end..]);
                let offsets = [0, t.start, cut.len() / 2, cut.len()];
                let offsets: Vec<usize> = offsets.iter().map(|&o| floor(&cut, o)).collect();
                run(&dialect, &cut, &offsets);
            }
        }
    }
}

#[test]
fn random_cuts_and_junk_never_panic() {
    let script = texts().join(";\n");
    let vocabulary = [
        "SELECT ",
        "FROM ",
        "WHERE ",
        "JOIN ",
        "ON ",
        "(",
        ")",
        ",",
        ".",
        "*",
        "'",
        "\"",
        "`",
        "[",
        "]",
        "$$",
        "$1",
        "@x",
        ":n",
        "?",
        "/*",
        "*/",
        "--",
        "\n",
        "CASE ",
        "WHEN ",
        "END ",
        "WITH ",
        "AS ",
        "INSERT ",
        "INTO ",
        "VALUES ",
        "UPDATE ",
        "SET ",
        "=",
        "::",
        "BEGIN ",
        "GO\n",
        "CREATE ",
        "TABLE ",
        "é",
        "名",
        " ",
        "a",
        "t1 ",
        "1",
        "1.5e",
        "-",
        ";",
        "UNION ",
        "OVER ",
        "PARTITION BY ",
        "DELIMITER $$\n",
        "#",
        "N'",
        "E'\\'",
        "LANGUAGE sql AS $$",
    ];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for dialect in dialects() {
        for _ in 0..600 {
            let a = floor(&script, rng.below(script.len()));
            let b = floor(&script, a + rng.below(400));
            let cut = &script[a..b.max(a)];
            let offsets = [
                0,
                floor(cut, rng.below(cut.len() + 1)),
                floor(cut, rng.below(cut.len() + 1)),
                cut.len(),
            ];
            run(&dialect, cut, &offsets);
        }
        for _ in 0..600 {
            let mut junk = String::new();
            for _ in 0..rng.below(60) {
                junk.push_str(vocabulary[rng.below(vocabulary.len())]);
            }
            let offsets = [0, floor(&junk, rng.below(junk.len() + 1)), junk.len()];
            run(&dialect, &junk, &offsets);
        }
    }
}

/// Inputs built to defeat a naive parser: deep nesting, long lists and chains.
fn pathological(n: usize) -> Vec<String> {
    vec![
        format!("SELECT {}", "(".repeat(n)),
        format!("SELECT {}1{} FROM t", "(".repeat(n), ")".repeat(n)),
        format!("{}SELECT", "(".repeat(n)),
        format!("SELECT {}b FROM t", "a, ".repeat(n)),
        format!("SELECT {}1", "CASE WHEN a THEN ".repeat(n)),
        format!("SELECT a{}", ".a".repeat(n)),
        format!("SELECT * FROM t WHERE {}a", "NOT ".repeat(n)),
        format!(
            "WITH {}SELECT * FROM c",
            "c AS (SELECT 1), ".repeat(n / 10 + 1)
        ),
        format!("SELECT * FROM {}t", "t JOIN ".repeat(n)),
        format!("SELECT * FROM {}", "(t JOIN ".repeat(n)),
        "BEGIN ".repeat(n),
        format!("SELECT f({}", "g(".repeat(n)),
        format!("SELECT * FROM t WHERE a IN ({})", "1, ".repeat(n)),
        "'".repeat(n),
        format!("SELECT {}", "x::".repeat(n)),
    ]
}

fn time_all(dialect: &Dialect, inputs: &[String]) -> Duration {
    let start = Instant::now();
    for text in inputs {
        let o = outline(dialect, text, 0..text.len());
        check_outline(&o, text);
        let ctx = context_at(dialect, text, text.len());
        check_context(&ctx, text);
        let ctx = context_at(dialect, text, text.len() / 2);
        check_context(&ctx, text);
    }
    start.elapsed()
}

#[test]
fn pathological_inputs_finish_in_linear_time() {
    for dialect in dialects() {
        let small = pathological(2_000);
        let big = pathological(16_000);
        // Warm up, then take the best of three.
        time_all(&dialect, &small);
        let best = |inputs: &[String]| {
            (0..3)
                .map(|_| time_all(&dialect, inputs))
                .min()
                .unwrap_or_default()
        };
        let t_small = best(&small);
        let t_big = best(&big);
        println!("{:?}: 8x input {t_small:?} -> {t_big:?}", dialect.engine);
        // Linear is about 8x; quadratic would be about 64x.
        assert!(
            t_big <= t_small * 24 + Duration::from_millis(100),
            "{:?}: {t_small:?} -> {t_big:?}",
            dialect.engine
        );
    }
}

/// A script of about `bytes` bytes of ordinary statements.
fn script(bytes: usize, separator: &str) -> String {
    let statements = [
        "SELECT o.id, o.total, c.name FROM sales.orders o JOIN customers c ON c.id = o.customer_id WHERE o.total > 100 AND c.region IN ('north', 'south') ORDER BY o.id",
        "UPDATE inventory SET quantity = quantity - 1, updated_at = CURRENT_TIMESTAMP WHERE product_id = 42",
        "INSERT INTO audit_log (actor, action, detail) VALUES ('system', 'update', 'quantity changed')",
    ];
    let mut out = String::with_capacity(bytes + 256);
    let mut i = 0;
    while out.len() < bytes {
        out.push_str(statements[i % statements.len()]);
        out.push_str(separator);
        i += 1;
    }
    out
}

/// The median of `runs` timings of `f`.
fn median(runs: usize, mut f: impl FnMut()) -> Duration {
    let mut times: Vec<Duration> = (0..runs)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed()
        })
        .collect();
    times.sort();
    times[times.len() / 2]
}

#[test]
fn context_on_a_one_mebibyte_script() {
    let debug = cfg!(debug_assertions);
    for dialect in dialects() {
        let separator = if dialect.engine == Engine::MsSql {
            "\n"
        } else {
            ";\n"
        };
        let mut text = script(1 << 20, separator);
        text.push_str("SELECT o.");
        let tail = text.len();
        text.push_str(" FROM sales.orders o");
        let middle = floor(&text, text.len() / 2);
        let t_end = median(3, || {
            let ctx = context_at(&dialect, &text, tail);
            assert_eq!(ctx.qualifier.len(), 1);
            assert_eq!(
                ctx.expected.first(),
                Some(&oxbase_insight::Expected::Column)
            );
        });
        let t_middle = median(3, || {
            let ctx = context_at(&dialect, &text, middle);
            check_context(&ctx, &text);
        });
        println!(
            "{:?}: 1 MiB script, cursor at the end {t_end:?}, in the middle {t_middle:?}",
            dialect.engine
        );
        // A T-SQL batch without GO is one statement: the whole MiB is parsed.
        let limit = match (debug, dialect.engine == Engine::MsSql) {
            (true, true) => Duration::from_secs(15),
            (true, false) => Duration::from_secs(5),
            (false, true) => Duration::from_millis(400),
            (false, false) => Duration::from_millis(100),
        };
        assert!(t_end < limit && t_middle < limit, "{:?}", dialect.engine);
    }
}

#[test]
fn context_on_a_ten_thousand_token_statement() {
    for dialect in dialects() {
        let mut text = String::from("SELECT ");
        let mut tokens = 1;
        while tokens < 10_000 {
            text.push_str("t.c + f(t.d, 1) AS x, ");
            tokens += 13;
        }
        text.push_str("t.‸ FROM orders t WHERE t.a = 1");
        let offset = text.find('‸').unwrap_or(0);
        let text = text.replace('‸', "");
        let elapsed = median(5, || {
            let ctx = context_at(&dialect, &text, offset);
            assert_eq!(ctx.expected, [oxbase_insight::Expected::Column]);
        });
        println!("{:?}: 10 000 tokens {elapsed:?}", dialect.engine);
        // `docs/m6-contracts.md` §4.2: under 5 ms in a release build.
        let limit = if cfg!(debug_assertions) {
            Duration::from_millis(500)
        } else {
            Duration::from_millis(5)
        };
        assert!(elapsed < limit, "{:?}: {elapsed:?}", dialect.engine);
    }
}
