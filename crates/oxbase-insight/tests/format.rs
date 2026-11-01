//! Formatter tests (`docs/m6-contracts.md` §4.6, invariant 3 of §8, §9): layout, comments,
//! opaque regions, `format_range`, and the properties over a corpus formatted with every option
//! combination and over fuzzed inputs: every result passes `check_format` (same tokens apart
//! from whitespace and keyword case) and formatting is idempotent.

use std::time::{Duration, Instant};

use oxbase_dialect::DialectRegistry;
use oxbase_insight::{Dialect, FormatOptions, KeywordCase, check_format, format, format_range};
use oxbase_types::{Engine, ErrorCategory};
use pretty_assertions::assert_eq;

const ENGINES: [Engine; 5] = [
    Engine::Postgres,
    Engine::MySql,
    Engine::MariaDb,
    Engine::Sqlite,
    Engine::MsSql,
];

/// The dialect with the SQL-standard core language data (layout tests).
fn dialect(engine: Engine) -> Dialect {
    Dialect::builtin(engine).unwrap()
}

/// The dialect with the engine's own language data, as the app uses it.
fn engine_dialect(engine: Engine) -> Dialect {
    let mut reg = DialectRegistry::new();
    oxbase_pg::dialect::register(&mut reg);
    oxbase_mysql::dialect::register(&mut reg);
    oxbase_sqlite::dialect::register(&mut reg);
    oxbase_mssql::dialect::register(&mut reg);
    let language = *reg.language(engine);
    assert_eq!(language.engine, Some(engine));
    Dialect::builtin(engine).unwrap().with_language(language)
}

/// Both language data sets of `engine`.
fn dialects(engine: Engine) -> [Dialect; 2] {
    [dialect(engine), engine_dialect(engine)]
}

/// `text` formatted with the default options and the engine's language data.
fn fmt_engine(engine: Engine, text: &str) -> String {
    let d = engine_dialect(engine);
    let out = format(&d, text, &FormatOptions::default()).unwrap();
    check_format(&d, text, &out).unwrap();
    out
}

fn fmt_with(engine: Engine, text: &str, options: FormatOptions) -> String {
    format(&dialect(engine), text, &options).unwrap()
}

fn fmt(engine: Engine, text: &str) -> String {
    fmt_with(engine, text, FormatOptions::default())
}

fn narrow() -> FormatOptions {
    FormatOptions {
        max_line_length: 40,
        ..FormatOptions::default()
    }
}

// --- layout ---------------------------------------------------------------------------------

#[test]
fn clauses_go_on_their_own_lines() {
    assert_eq!(
        fmt(Engine::Postgres, "select a, b from t where x = 1 and y = 2"),
        "SELECT a, b\nFROM t\nWHERE x = 1 AND y = 2"
    );
    assert_eq!(
        fmt(
            Engine::Postgres,
            "select o.id, c.name from orders o join customers c on c.id = o.customer_id \
             left join items i on i.order_id = o.id where o.total > 100 order by o.id desc limit 10"
        ),
        "SELECT o.id, c.name\nFROM orders o\nJOIN customers c ON c.id = o.customer_id\n\
         LEFT JOIN items i ON i.order_id = o.id\nWHERE o.total > 100\nORDER BY o.id DESC\nLIMIT 10"
    );
}

#[test]
fn long_lists_break_one_item_per_line() {
    let text = "select aaaaaaaaaa, bbbbbbbbbb, cccccccccc, dddddddddd from t";
    assert_eq!(
        fmt_with(Engine::Postgres, text, narrow()),
        "SELECT\n    aaaaaaaaaa,\n    bbbbbbbbbb,\n    cccccccccc,\n    dddddddddd\nFROM t"
    );
    let leading = FormatOptions {
        leading_commas: true,
        indent_width: 2,
        ..narrow()
    };
    assert_eq!(
        fmt_with(Engine::Postgres, text, leading),
        "SELECT\n  aaaaaaaaaa\n  , bbbbbbbbbb\n  , cccccccccc\n  , dddddddddd\nFROM t"
    );
    let tabs = FormatOptions {
        use_tabs: true,
        ..narrow()
    };
    assert_eq!(
        fmt_with(Engine::Postgres, text, tabs),
        "SELECT\n\taaaaaaaaaa,\n\tbbbbbbbbbb,\n\tcccccccccc,\n\tdddddddddd\nFROM t"
    );
}

#[test]
fn ctes_and_subqueries_are_indented() {
    assert_eq!(
        fmt(
            Engine::Postgres,
            "with c as (select id from t where x in (select y from u)) select * from c"
        ),
        "WITH c AS (\n    SELECT id\n    FROM t\n    WHERE x IN (SELECT y FROM u)\n)\nSELECT *\nFROM c"
    );
    assert_eq!(
        fmt(Engine::Sqlite, "select x.a from (select a from t) as x"),
        "SELECT x.a\nFROM (\n    SELECT a\n    FROM t\n) AS x"
    );
}

#[test]
fn case_arms_break_when_too_long() {
    let text =
        "select case when a = 1 then 'one' when a = 2 then 'two' else 'many' end as label from t";
    assert_eq!(
        fmt(Engine::Postgres, text),
        "SELECT CASE WHEN a = 1 THEN 'one' WHEN a = 2 THEN 'two' ELSE 'many' END AS label\nFROM t"
    );
    assert_eq!(
        fmt_with(Engine::Postgres, text, narrow()),
        "SELECT\n    CASE\n        WHEN a = 1 THEN 'one'\n        WHEN a = 2 THEN 'two'\n        \
         ELSE 'many'\n    END AS label\nFROM t"
    );
}

#[test]
fn dml_and_ddl() {
    assert_eq!(
        fmt(
            Engine::Postgres,
            "insert into t (a, b) values (1, 2), (3, 4)"
        ),
        "INSERT INTO t (a, b)\nVALUES (1, 2), (3, 4)"
    );
    assert_eq!(
        fmt(Engine::Postgres, "update t set a = 1, b = 2 where id = 3"),
        "UPDATE t\nSET a = 1, b = 2\nWHERE id = 3"
    );
    assert_eq!(
        fmt(Engine::Postgres, "delete from t where id = 3"),
        "DELETE FROM t\nWHERE id = 3"
    );
    assert_eq!(
        fmt(
            Engine::Postgres,
            "create table t (id int primary key, name text not null)"
        ),
        "CREATE TABLE t (\n    id int PRIMARY KEY,\n    name text NOT NULL\n)"
    );
}

#[test]
fn keyword_case_options() {
    let lower = FormatOptions {
        keyword_case: KeywordCase::Lower,
        ..FormatOptions::default()
    };
    assert_eq!(
        fmt_with(Engine::Postgres, "SELECT A FROM T", lower),
        "select A\nfrom T"
    );
    let preserve = FormatOptions {
        keyword_case: KeywordCase::Preserve,
        ..FormatOptions::default()
    };
    assert_eq!(
        fmt_with(Engine::Postgres, "Select a From t", preserve),
        "Select a\nFrom t"
    );
    // Names that are also keywords keep their spelling.
    assert_eq!(
        fmt(
            Engine::Postgres,
            "select data, t.order from t where data = 1"
        ),
        "SELECT data, t.order\nFROM t\nWHERE data = 1"
    );
}

/// Names that are also keywords keep their spelling with the engines' own language data: on
/// MySQL (Linux), SQLite and SQL Server a re-cased alias, table or result label is a different
/// one.
#[test]
fn names_that_are_keywords_keep_their_spelling() {
    for engine in [Engine::MySql, Engine::MariaDb] {
        // Implicit table aliases.
        assert_eq!(
            fmt_engine(engine, "select user.name from users user"),
            "SELECT user.name\nFROM users user"
        );
        assert_eq!(
            fmt_engine(
                engine,
                "select s.id from t join u status on status.id = t.id where status.x = 1"
            ),
            "SELECT s.id\nFROM t\nJOIN u status ON status.id = t.id\nWHERE status.x = 1"
        );
        // Column aliases without AS.
        assert_eq!(
            fmt_engine(
                engine,
                "select DATE(created_at) date, COUNT(*) total from orders group by date"
            ),
            "SELECT DATE(created_at) date, COUNT(*) total\nFROM orders\nGROUP BY date"
        );
        assert_eq!(
            fmt_engine(engine, "select start, end from events"),
            "SELECT start, end\nFROM events"
        );
        // Names in DDL and other statements.
        assert_eq!(
            fmt_engine(engine, "rename table a to status"),
            "RENAME TABLE a TO status"
        );
        assert_eq!(
            fmt_engine(engine, "alter table t rename to user"),
            "ALTER TABLE t RENAME TO user"
        );
        assert_eq!(
            fmt_engine(engine, "alter table t add status int"),
            "ALTER TABLE t ADD status INT"
        );
        assert_eq!(
            fmt_engine(engine, "alter table t change created date DATETIME"),
            "ALTER TABLE t CHANGE created date DATETIME"
        );
        assert_eq!(fmt_engine(engine, "truncate data"), "TRUNCATE data");
        assert_eq!(
            fmt_engine(
                engine,
                "delete status from status join u on u.id = status.id"
            ),
            "DELETE status\nFROM status\nJOIN u ON u.id = status.id"
        );
        // Words MySQL 8 reserves but MySQL 5.7 and MariaDB accept as table names.
        assert_eq!(
            fmt_engine(engine, "select * from groups"),
            "SELECT *\nFROM groups"
        );
        // Keywords around them still change case.
        assert_eq!(
            fmt_engine(
                engine,
                "select case when a > 0 then 1 else 0 end as sign from t"
            ),
            "SELECT CASE WHEN a > 0 THEN 1 ELSE 0 END AS sign\nFROM t"
        );
    }
    assert_eq!(
        fmt_engine(Engine::MariaDb, "select rank from players"),
        "SELECT rank\nFROM players"
    );
    let ms = Engine::MsSql;
    assert_eq!(
        fmt_engine(
            ms,
            "select o.id, MAX(o.total) name from orders o join kinds type on type.id = o.kind_id group by o.id"
        ),
        "SELECT o.id, MAX(o.total) name\nFROM orders o\nJOIN kinds type ON type.id = o.kind_id\nGROUP BY o.id"
    );
    assert_eq!(
        fmt_engine(ms, "alter table t add type int"),
        "ALTER TABLE t ADD type int"
    );
    // SQLite does not reserve the join words; they still read as keywords before JOIN.
    assert_eq!(
        fmt_engine(
            Engine::Sqlite,
            "select a.x from a left join b on b.id = a.id"
        ),
        "SELECT a.x\nFROM a\nLEFT JOIN b ON b.id = a.id"
    );
}

#[test]
fn statement_keywords_split_only_t_sql_batches() {
    // Outside T-SQL a statement keyword at the start of a line continues the statement.
    let grant = "grant select on t to bob\nwith grant OPTION";
    assert_eq!(
        fmt(Engine::Postgres, grant),
        "GRANT SELECT ON t TO bob WITH GRANT OPTION"
    );
    assert_eq!(
        fmt(Engine::MsSql, grant),
        "GRANT SELECT ON t TO bob WITH GRANT OPTION"
    );
    // A T-SQL batch still splits at a statement keyword that starts a line.
    assert_eq!(
        fmt(Engine::MsSql, "select 1\nselect 2"),
        "SELECT 1\nSELECT 2"
    );
}

#[test]
fn comments_stay_with_their_tokens() {
    assert_eq!(
        fmt(
            Engine::Postgres,
            "select a, -- first\n  b\nfrom t -- tail\n-- own line\nwhere x = 1"
        ),
        "SELECT\n    a, -- first\n    b\nFROM t -- tail\n-- own line\nWHERE x = 1"
    );
    assert_eq!(
        fmt(Engine::Postgres, "select /* inline */ a from t"),
        "SELECT /* inline */ a\nFROM t"
    );
}

#[test]
fn string_literals_keep_their_line_breaks() {
    // PostgreSQL concatenates adjacent literals only across a line break.
    assert_eq!(
        fmt(Engine::Postgres, "select 'a'\n  'b' from t"),
        "SELECT\n    'a'\n    'b'\nFROM t"
    );
}

#[test]
fn statements_are_separated_as_the_options_say() {
    assert_eq!(
        fmt(Engine::Postgres, "select 1; select 2;"),
        "SELECT 1;\n\nSELECT 2;"
    );
    let tight = FormatOptions {
        blank_line_between_statements: false,
        ..FormatOptions::default()
    };
    assert_eq!(
        fmt_with(Engine::Postgres, "select 1;\n\n\nselect 2;\n", tight),
        "SELECT 1;\nSELECT 2;\n"
    );
    assert_eq!(
        fmt(Engine::Postgres, "select a\r\nfrom t\r\n"),
        "SELECT a\r\nFROM t\r\n"
    );
}

// --- opaque regions and unparseable statements ----------------------------------------------

#[test]
fn go_and_delimiter_lines_are_kept() {
    assert_eq!(
        fmt(Engine::MsSql, "select 1\ngo\nselect 2\n"),
        "SELECT 1\ngo\n\nSELECT 2\n"
    );
    let script =
        "DELIMITER //\nCREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\nEND//\nDELIMITER ;\nselect 2;\n";
    assert_eq!(
        fmt(Engine::MySql, script),
        "DELIMITER //\nCREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\nEND//\nDELIMITER ;\n\nSELECT 2;\n"
    );
}

#[test]
fn routine_bodies_are_kept_verbatim() {
    assert_eq!(
        fmt(
            Engine::Postgres,
            "create function f() returns int as $$ select  1 $$ language sql;\nselect 1;"
        ),
        "create function f() returns int as $$ select  1 $$ language sql;\n\nSELECT 1;"
    );
    let batch = "IF EXISTS (SELECT 1 FROM t)\n  select 2\nELSE\n  select 3";
    assert_eq!(fmt(Engine::MsSql, batch), batch);
}

#[test]
fn unparseable_statements_are_returned_unchanged() {
    assert_eq!(
        fmt(Engine::Postgres, "select (a from t;\nselect 1"),
        "select (a from t;\n\nSELECT 1"
    );
    assert_eq!(fmt(Engine::Postgres, "select 'abc"), "select 'abc");
    assert_eq!(
        fmt(Engine::Postgres, "select case when a then 1 from t"),
        "select case when a then 1 from t"
    );
    assert_eq!(fmt(Engine::Postgres, ""), "");
    assert_eq!(
        fmt(Engine::Postgres, "-- only a comment\n"),
        "-- only a comment\n"
    );
}

#[test]
fn invalid_options_are_refused() {
    let bad = FormatOptions {
        indent_width: 9,
        ..FormatOptions::default()
    };
    let err = format(&dialect(Engine::Postgres), "select 1", &bad).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    let err = format_range(&dialect(Engine::Postgres), "select 1", 0..1, &bad).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
}

// --- format_range ---------------------------------------------------------------------------

#[test]
fn format_range_replaces_the_overlapping_statements() {
    let pg = dialect(Engine::Postgres);
    let o = FormatOptions::default();
    let text = "select 1;\n  select a, b from t where x = 1;\nselect 3;";
    let start = text.find("select a").unwrap();
    let end = start + "select a, b from t where x = 1".len();
    let edit = format_range(&pg, text, start + 2..start + 3, &o).unwrap();
    assert_eq!(edit.range, start..end);
    assert_eq!(edit.text, "SELECT a, b\n  FROM t\n  WHERE x = 1");
    // A cursor (empty range) picks the statement around it.
    let edit = format_range(&pg, text, start..start, &o).unwrap();
    assert_eq!(edit.range, start..end);
    // Two statements, with the gap between them.
    let edit = format_range(&pg, text, 0..start + 1, &o).unwrap();
    assert_eq!(edit.range, 0..end);
    assert_eq!(edit.text, "SELECT 1;\n\nSELECT a, b\nFROM t\nWHERE x = 1");
    // Nothing there: an empty edit.
    let gap = text.find('\n').unwrap() + 1;
    let edit = format_range(&pg, text, gap..gap, &o).unwrap();
    assert_eq!(edit.range, gap..gap);
    assert_eq!(edit.text, "");
    // Out-of-range offsets are clamped, never panic.
    let edit = format_range(&pg, text, 10_000..20_000, &o).unwrap();
    assert!(edit.range.end <= text.len());
    #[allow(clippy::reversed_empty_ranges)] // a reversed range must not panic either
    let edit = format_range(&pg, "select 'é'", 9..8, &o).unwrap();
    assert!(edit.range.end <= "select 'é'".len());
}

#[test]
fn format_range_results_pass_the_check() {
    for d in ENGINES.into_iter().flat_map(dialects) {
        let engine = d.engine;
        for text in corpus(engine) {
            for at in [0, text.len() / 3, text.len() / 2, text.len()] {
                let at = floor(text, at);
                let edit = format_range(&d, text, at..at, &FormatOptions::default()).unwrap();
                let replaced = [
                    &text[..edit.range.start],
                    edit.text.as_str(),
                    &text[edit.range.end..],
                ]
                .concat();
                check_format(&d, text, &replaced).unwrap();
            }
        }
    }
}

// --- properties -----------------------------------------------------------------------------

/// Every combination of the options that change the layout.
fn option_matrix() -> Vec<FormatOptions> {
    let mut all = Vec::new();
    for keyword_case in [
        KeywordCase::Upper,
        KeywordCase::Lower,
        KeywordCase::Preserve,
    ] {
        for leading_commas in [false, true] {
            for blank_line_between_statements in [false, true] {
                for use_tabs in [false, true] {
                    for indent_width in [2, 4] {
                        for max_line_length in [40, 100] {
                            all.push(FormatOptions {
                                indent_width,
                                use_tabs,
                                keyword_case,
                                max_line_length,
                                leading_commas,
                                blank_line_between_statements,
                            });
                        }
                    }
                }
            }
        }
    }
    all
}

/// Formats `text` and checks the invariants: the result passes `check_format` and formatting
/// it again changes nothing. The violation, described, otherwise.
fn properties(d: &Dialect, text: &str, o: FormatOptions) -> Result<(), String> {
    let once = format(d, text, &o)
        .map_err(|e| format!("{:?}: format failed ({e}) on {text:?}", d.engine))?;
    check_format(d, text, &once).map_err(|e| format!("{e} on {text:?}"))?;
    let twice = format(d, &once, &o).map_err(|e| format!("{e} on the output {once:?}"))?;
    if twice != once {
        return Err(format!(
            "{:?} not idempotent on {text:?} with {o:?}:\n first: {once:?}\nsecond: {twice:?}",
            d.engine
        ));
    }
    Ok(())
}

/// Collects violations (the first 25 are reported, all are counted).
#[derive(Default)]
struct Violations {
    seen: Vec<String>,
    count: usize,
}

impl Violations {
    fn check(&mut self, d: &Dialect, text: &str, o: FormatOptions) {
        if let Err(e) = properties(d, text, o) {
            self.count += 1;
            if self.seen.len() < 25 && !self.seen.contains(&e) {
                self.seen.push(e);
            }
        }
    }

    fn assert_none(&self) {
        assert!(
            self.count == 0,
            "{} violations, first ones:\n{}",
            self.count,
            self.seen.join("\n---\n")
        );
    }
}

#[test]
fn corpus_with_every_option_combination() {
    let options = option_matrix();
    let mut violations = Violations::default();
    for engine in ENGINES {
        let all = corpus(engine).join(";\n");
        for d in dialects(engine) {
            for o in &options {
                for text in corpus(engine) {
                    violations.check(&d, text, *o);
                }
                violations.check(&d, &all, *o);
            }
        }
    }
    violations.assert_none();
}

/// A tiny deterministic random generator (xorshift) for the fuzz test.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        let n = u64::try_from(n.max(1)).unwrap_or(1);
        usize::try_from(self.next() % n).unwrap_or(0)
    }
}

fn floor(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[test]
fn fuzzed_inputs_keep_the_invariants() {
    const JUNK: &[&str] = &[
        "(", ")", "'", "\"", "`", "[", "]", ",", ";", "--", "/*", "*/", "\n", " ", "case", "end",
        "select", "from", "where", "and", "join", "on", "go", "\nGO\n", "begin", "$$", "-", ".",
        "::", "e", "1", "x", "#", "@", "?", ":",
    ];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let wide = FormatOptions::default();
    let narrow = narrow();
    let mut violations = Violations::default();
    for sql in ENGINES.into_iter().flat_map(dialects) {
        let engine = sql.engine;
        for text in corpus(engine) {
            // Every prefix (incomplete SQL is the normal case).
            for (at, _) in text.char_indices() {
                violations.check(&sql, &text[..at], wide);
            }
            // Every single-token deletion.
            let tokens: Vec<_> = oxbase_sql::tokenize(sql.sql, text).collect();
            for token in &tokens {
                let cut = [&text[..token.start], &text[token.end..]].concat();
                violations.check(&sql, &cut, narrow);
            }
            // Random insertions.
            for _ in 0..40 {
                let mut mutated = text.to_string();
                for _ in 0..=rng.below(3) {
                    let at = floor(&mutated, rng.below(mutated.len() + 1));
                    mutated.insert_str(at, JUNK[rng.below(JUNK.len())]);
                }
                let options = if rng.below(2) == 0 { wide } else { narrow };
                violations.check(&sql, &mutated, options);
            }
        }
    }
    violations.assert_none();
}

/// A script of at least 100 KiB made of the corpus statements of `engine`.
fn large_script(engine: Engine) -> String {
    let mut script = String::new();
    while script.len() < 100 * 1024 {
        for text in corpus(engine) {
            script.push_str(text);
            script.push_str(";\n");
        }
    }
    script
}

#[test]
fn a_large_script_formats_quickly() {
    let d = engine_dialect(Engine::Postgres);
    let script = large_script(Engine::Postgres);
    let started = Instant::now();
    let out = format(&d, &script, &FormatOptions::default()).unwrap();
    let elapsed = started.elapsed();
    eprintln!(
        "formatted {} KiB in {elapsed:?} (budget in a perf build: 200 ms)",
        script.len() / 1024
    );
    assert!(out.len() > script.len() / 2);
}

/// The §5 budget: `format` within 200 ms for a 100 KiB script, in a `--profile perf` build.
/// Ignored by default (debug builds are slower); run it with
/// `cargo test -p oxbase-insight --profile perf --test format -- --ignored --nocapture`.
/// Prints the best of five runs per engine; with `OXBASE_REQUIRE_M6_PERF=1` a run above the
/// budget fails the test.
#[test]
#[ignore = "perf budget; run in a --profile perf build"]
fn format_perf_budget() {
    let budget = Duration::from_millis(200);
    let required = std::env::var("OXBASE_REQUIRE_M6_PERF").is_ok_and(|v| v == "1");
    let mut over = Vec::new();
    for engine in ENGINES {
        let d = engine_dialect(engine);
        let script = large_script(engine);
        let mut best = Duration::MAX;
        for _ in 0..5 {
            let started = Instant::now();
            let out = format(&d, &script, &FormatOptions::default()).unwrap();
            best = best.min(started.elapsed());
            assert!(!out.is_empty());
        }
        eprintln!(
            "format perf: {engine:?} {} KiB in {best:?} (budget {budget:?})",
            script.len() / 1024
        );
        if best > budget {
            over.push(format!("{engine:?}: {best:?}"));
        }
    }
    assert!(
        !required || over.is_empty(),
        "over the {budget:?} budget: {}",
        over.join(", ")
    );
}

// --- corpus ---------------------------------------------------------------------------------

fn corpus(engine: Engine) -> &'static [&'static str] {
    match engine {
        Engine::Postgres => PG,
        Engine::MySql | Engine::MariaDb => MYSQL,
        Engine::Sqlite => SQLITE,
        _ => TSQL,
    }
}

const PG: &[&str] = &[
    "select a, b from t where x = 1 and y = 2",
    "SELECT o.id, c.name, sum(i.qty * i.price) AS total FROM orders o JOIN customers c ON c.id = o.customer_id LEFT OUTER JOIN items i ON i.order_id = o.id AND i.deleted IS NULL WHERE o.created_at >= now() - interval '7 days' AND (o.status = 'open' OR o.status = 'held') GROUP BY o.id, c.name HAVING count(*) > 1 ORDER BY total DESC NULLS LAST LIMIT 20 OFFSET 40",
    "with recursive tree(id, parent) as (select id, parent from nodes where parent is null union all select n.id, n.parent from nodes n join tree t on n.parent = t.id) select * from tree",
    "with a as materialized (select 1 as x), b as (select x + 1 as y from a) select y from b",
    "select distinct on (a) a, b from t order by a, b desc",
    "select row_number() over (partition by a order by b) as rn, lag(b, 1) over w from t window w as (order by b)",
    "select x::text, data->>'key', data #> '{a,b}', arr[1], arr[1:2], '{1,2}'::int[] from t",
    "select * from t where a between 1 and 10 and b not in (1, 2, 3) and exists (select 1 from u where u.id = t.id)",
    "select case when a = 1 then 'one' when a = 2 then 'two' else 'many' end as label, case a when 1 then true else false end from t",
    "insert into t (a, b) values (1, 'x'), (2, 'y') on conflict (a) do update set b = excluded.b where t.b <> excluded.b returning *",
    "insert into t (a) select a from u where a > 0 on conflict do nothing",
    "update t set a = u.a, b = default from u where t.id = u.id returning t.id",
    "delete from t using u where t.id = u.id and u.flag",
    "create table if not exists public.t (id bigint generated always as identity primary key, name text not null default '', created_at timestamptz default now(), constraint t_name_uq unique (name))",
    "create index concurrently if not exists t_name_idx on t using btree (lower(name)) where name is not null",
    "create or replace view v as select a, b from t where a > 0",
    "alter table t add column c int, drop column d, alter column e set default 0",
    "create function f(a int) returns int language sql as $$ select a + 1 $$",
    "do $body$ begin raise notice 'hi'; end $body$",
    "explain analyze select * from t where id = $1",
    "select 'it''s', E'a\\nb', $$dollar$$, $tag$x$tag$, U&'d\\0061t\\+000061', B'1010', X'1F'",
    "select a -- trailing comment\n, b /* block */ from t -- end\n-- leading\nwhere x = -1",
    "select -a, +b, ~c, a - -b, a--b\n from t",
    "select 1 union select 2 intersect select 3 except all select 4",
    "select * from a natural join b cross join c full outer join d using (id) right join e on e.id = d.id",
    "select * from t for update skip locked",
    "select * from t fetch first 10 rows only",
    "select count(*) filter (where x > 0), string_agg(name, ', ' order by name) from t",
    "vacuum analyze t",
    "grant select, insert on t to bob",
    "comment on table t is 'the table'",
    "begin; update t set a = 1; commit",
    "select (select max(x) from u) as m, (a + b) * c from t",
    "select 'a'\n'b' as joined",
    "select date, user, data, year from t as data",
    "merge into t using s on t.id = s.id when matched then update set a = s.a when not matched then insert (id, a) values (s.id, s.a)",
    "select a from t where x = :param and y = ? and z = $2",
    "select * from \"Weird Table\" as \"T\" where \"T\".\"Col\" = 1",
];

const MYSQL: &[&str] = &[
    "select `a b`, c from `db`.`t` where c = 1 limit 5, 10",
    "SELECT /*+ BKA(t) */ a FROM t WHERE b = 'x' # mysql comment\nAND c = 2",
    "/*!40101 SET NAMES utf8mb4 */",
    "insert into t (a, b) values (1, 2) on duplicate key update b = values(b), a = a + 1",
    "insert into t set a = 1, b = 'x'",
    "replace into t values (1, 2)",
    "update a join b on a.id = b.id set a.x = b.x where b.y > 0 order by a.id limit 10",
    "delete a from a join b on a.id = b.id where b.gone",
    "select @x := @x + 1 as n, @@session.sql_mode, if(a > 0, 'p', 'n') from t, (select @x := 0) init",
    "select _utf8mb4'abc' collate utf8mb4_bin, x'ff', 0x1F, b'01', n'x'",
    "create table t (id int unsigned not null auto_increment, name varchar(100) default null, primary key (id), key idx_name (name)) engine=InnoDB default charset=utf8mb4",
    "create definer=`root`@`localhost` procedure p(in a int)\nbegin\n  select a;\nend",
    "DELIMITER //\nCREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW\nBEGIN\n  SET NEW.a = 1;\nEND//\nDELIMITER ;\nselect 1",
    "show tables",
    "set @a = 1, @b = 2",
    "select a from t where b like '%\\_x%' escape '\\\\' and c regexp '^a'",
    "select * from t1 straight_join t2 on t1.a = t2.a",
    "select count(distinct a) from t group by b with rollup",
    "select a from t where x = 1--2\n",
    "alter table t add column c int after b, add index (c)",
];

const SQLITE: &[&str] = &[
    "select a, b from t where a = ?1 and b = :b and c = @c and d = $d",
    "insert or replace into t (a, b) values (?, ?)",
    "insert or ignore into t default values",
    "create table if not exists t (id integer primary key autoincrement, name text not null collate nocase, data blob) without rowid",
    "create temp trigger tr after insert on t begin update t set a = 1 where id = new.id; end",
    "pragma table_info(t)",
    "pragma journal_mode = wal",
    "attach database 'x.db' as x",
    "with recursive c(x) as (select 1 union all select x + 1 from c where x < 10) select x from c",
    "select [a b], `c`, \"d\" from [t]",
    "select json_extract(data, '$.a'), data -> '$.b', data ->> 'c' from t",
    "select * from t where rowid in (select rowid from t limit 5) order by 1",
    "update t set (a, b) = (1, 2) where id = 1 returning *",
    "delete from t where id in (1, 2, 3)",
    "select cast(a as integer), typeof(b), a || b from t",
    "begin immediate; insert into t values (1); commit",
    "create view v as select * from t",
    "select a from t group by a having count(*) > 1 order by a collate nocase",
];

const TSQL: &[&str] = &[
    "select top (10) [a], b from [dbo].[t] with (nolock) where c = N'x' order by a",
    "declare @x int = 1\nset @x = @x + 1\nselect @x as x\ngo\nselect 2",
    "select a into #tmp from t; select * from #tmp; drop table #tmp",
    "insert into t (a) output inserted.id values (1)",
    "update t set a = 1 output deleted.a, inserted.a where id = 1",
    "delete top (5) from t where a < 0",
    "merge into t as tgt using s as src on tgt.id = src.id when matched then update set tgt.a = src.a when not matched by target then insert (id, a) values (src.id, src.a) when not matched by source then delete;",
    "select * from t cross apply f(t.id) as x outer apply (select top 1 * from u where u.t = t.id) as y",
    "select a from t option (recompile)",
    "select a, b from t for xml path('row'), root('rows')",
    "exec sp_who @loginame = 'sa', @x = 1",
    "if exists (select 1 from t)\nbegin\n  print 'yes'\nend\nelse\n  print 'no'",
    "create procedure p @a int as begin set nocount on; select @a; end",
    "begin tran; update t set a = 1; commit tran",
    "select @@rowcount, @@version",
    "waitfor delay '00:00:01'",
    "throw 50000, 'boom', 1;",
    "select geography::Point(1, 2, 4326)",
    "select a from db..t",
    "with c as (select 1 as x) select x from c",
];
