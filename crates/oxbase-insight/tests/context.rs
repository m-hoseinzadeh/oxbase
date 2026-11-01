//! Cursor context tables (`docs/m6-contracts.md` §4.2). `‸` marks the cursor; each row runs
//! on every engine listed for it.

use oxbase_insight::{
    Clause, CursorContext, Dialect, Expected, RelationSource, ScopeRelation, StatementClass,
    context_at,
};
use oxbase_types::{Engine, IdentCase};

const CURSOR: char = '‸';

const ALL: &[Engine] = &[
    Engine::Postgres,
    Engine::MySql,
    Engine::MariaDb,
    Engine::Sqlite,
    Engine::MsSql,
];
const PG: &[Engine] = &[Engine::Postgres];
const MY: &[Engine] = &[Engine::MySql, Engine::MariaDb];
const MS: &[Engine] = &[Engine::MsSql];
const LITE: &[Engine] = &[Engine::Sqlite];
const PG_MS: &[Engine] = &[Engine::Postgres, Engine::MsSql];
const PG_MY: &[Engine] = &[Engine::Postgres, Engine::MySql, Engine::MariaDb];
const PG_MY_MS: &[Engine] = &[
    Engine::Postgres,
    Engine::MySql,
    Engine::MariaDb,
    Engine::MsSql,
];
const NO_LITE: &[Engine] = &[
    Engine::Postgres,
    Engine::MySql,
    Engine::MariaDb,
    Engine::MsSql,
];
const ANSI_QUOTES: &[Engine] = &[Engine::Postgres, Engine::Sqlite, Engine::MsSql];
const BACKTICKS: &[Engine] = &[Engine::MySql, Engine::MariaDb, Engine::Sqlite];
const BRACKETS: &[Engine] = &[Engine::Sqlite, Engine::MsSql];

const K: Expected = Expected::Keyword;
const SN: Expected = Expected::Snippet;
const C: Expected = Expected::Column;
const F: Expected = Expected::Function;
const R: Expected = Expected::Relation;
const S: Expected = Expected::Schema;
const T: Expected = Expected::Type;
const P: Expected = Expected::Procedure;
const D: Expected = Expected::Database;

/// The context at the cursor marker of `marked`.
fn at(engine: Engine, marked: &str) -> CursorContext {
    let offset = marked.find(CURSOR).expect("the text has a cursor marker");
    let text = marked.replacen(CURSOR, "", 1);
    let dialect = Dialect::builtin(engine).expect("a SQL engine");
    context_at(&dialect, &text, offset)
}

/// The name a scope relation goes by: its alias, else its own name.
fn name_of(ctx: &CursorContext, relation: &ScopeRelation) -> String {
    if let Some(alias) = &relation.alias {
        return alias.text.clone();
    }
    match &relation.source {
        RelationSource::Named(parts) | RelationSource::Function(parts) => {
            parts.last().map_or_else(String::new, |p| p.text.clone())
        }
        RelationSource::Cte { query, index } => {
            ctx.outline.queries[*query].ctes[*index].name.text.clone()
        }
        RelationSource::Derived { .. } => "(derived)".to_owned(),
        RelationSource::Values { .. } => "(values)".to_owned(),
    }
}

fn scope(ctx: &CursorContext) -> Vec<String> {
    ctx.scope.iter().map(|r| name_of(ctx, r)).collect()
}

fn sources(ctx: &CursorContext) -> Vec<String> {
    ctx.column_sources(IdentCase::Insensitive)
        .into_iter()
        .map(|r| match &r.source {
            RelationSource::Named(parts) => {
                parts.last().map_or_else(String::new, |p| p.text.clone())
            }
            _ => name_of(ctx, r),
        })
        .collect()
}

/// (engines, text with cursor, expected kinds the context must start with; empty = none).
const TABLE: &[(&[Engine], &str, &[Expected])] = &[
    // Statement starts.
    (ALL, "‸", &[K, SN]),
    (ALL, "  \n‸", &[K, SN]),
    (ALL, "SELECT 1;\n‸", &[K, SN]),
    (ALL, "SEL‸", &[K, SN]),
    (ALL, "SELECT 1; SEL‸", &[K, SN]),
    (ALL, "SELECT 1;\n‸\nSELECT 2", &[K, SN]),
    // Select lists.
    (ALL, "SELECT ‸", &[C, F, K]),
    (ALL, "SELECT a, ‸", &[C, F, K]),
    (ALL, "SELECT a, ‸ FROM orders", &[C, F, K]),
    (ALL, "SELECT ‸ FROM orders o", &[C, F, K]),
    (ALL, "SELECT ord‸ FROM orders o", &[C, F, K]),
    (ALL, "SELECT o.‸ FROM orders o", &[C]),
    (ALL, "SELECT o.na‸ FROM orders o", &[C]),
    (ALL, "SELECT s.‸", &[R, C]),
    (ALL, "SELECT x.‸ FROM orders o", &[C, F]),
    (ALL, "SELECT count(‸", &[C, F, K]),
    (ALL, "SELECT count(DISTINCT ‸) FROM t", &[C, F, K]),
    (ALL, "SELECT coalesce(a, ‸) FROM t", &[C, F, K]),
    (ALL, "SELECT CASE WHEN ‸", &[C, F, K]),
    (ALL, "SELECT CASE WHEN a THEN ‸ END FROM t", &[C, F, K]),
    (ALL, "SELECT a + ‸ FROM t", &[C, F, K]),
    (ALL, "SELECT DISTINCT ‸", &[C, F, K]),
    (ALL, "SELECT a AS x‸", &[]),
    (ALL, "SELECT a AS ‸", &[]),
    (ALL, "SELECT a ‸", &[K]),
    (ALL, "SELECT a fr‸", &[K]),
    (ALL, "SELECT * ‸", &[K]),
    (ALL, "SELECT CAST(a AS ‸", &[T]),
    (ALL, "SELECT CAST(a AS var‸) FROM t", &[T]),
    (PG, "SELECT a::‸ FROM t", &[T]),
    (MS, "SELECT TOP 10 ‸ FROM t", &[C, F, K]),
    (MS, "SELECT total = ‸ FROM t", &[C, F, K]),
    (ALL, "SELECT (SELECT x.‸ FROM t2 x) FROM t1", &[C]),
    (ALL, "SELECT 'abc‸'", &[]),
    (ALL, "SELECT 1 -- note ‸", &[]),
    (ALL, "SELECT /* c‸ */ 1", &[]),
    (ALL, "SELECT 12‸", &[]),
    (MS, "SELECT @va‸", &[]),
    (MY, "SELECT @va‸", &[]),
    (PG, "SELECT $1‸", &[]),
    (
        ALL,
        "SELECT a FROM t WHERE a IN (SELECT ‸ FROM u)",
        &[C, F, K],
    ),
    (
        ALL,
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.‸)",
        &[C],
    ),
    (
        ALL,
        "SELECT row_number() OVER (PARTITION BY ‸) FROM t",
        &[C, F, K],
    ),
    (
        ALL,
        "SELECT row_number() OVER (ORDER BY ‸) FROM t",
        &[C, F, K],
    ),
    // FROM and joins.
    (ALL, "SELECT * FROM ‸", &[R, S]),
    (ALL, "SELECT * FROM ord‸", &[R, S]),
    (ALL, "SELECT * FROM ‸ WHERE a = 1", &[R, S]),
    (ALL, "SELECT * FROM sales.‸", &[R]),
    (ALL, "SELECT * FROM sales.ord‸", &[R]),
    (PG_MS, "SELECT * FROM db.sales.‸", &[R]),
    (ALL, "SELECT * FROM orders ‸", &[K]),
    (ALL, "SELECT * FROM orders o ‸", &[K]),
    (ALL, "SELECT * FROM orders o, ‸", &[R, S]),
    (ALL, "SELECT * FROM orders o JOIN ‸", &[R, S]),
    (ALL, "SELECT * FROM orders o LEFT JOIN ‸", &[R, S]),
    (ALL, "SELECT * FROM orders o LEFT OUTER JOIN ‸", &[R, S]),
    (ALL, "SELECT * FROM orders o LEFT ‸", &[K]),
    (
        ALL,
        "SELECT * FROM orders o JOIN customers c ON ‸",
        &[C, F, K],
    ),
    (ALL, "SELECT * FROM orders o JOIN customers c ON c.‸", &[C]),
    (
        ALL,
        "SELECT * FROM orders o JOIN customers c ON c.id = o.‸",
        &[C],
    ),
    (ALL, "SELECT * FROM orders o JOIN customers c ‸", &[K]),
    (ALL, "SELECT * FROM orders o WHERE ‸", &[C, F, K]),
    (
        ALL,
        "SELECT * FROM orders o WHERE o.total > 10 AND ‸",
        &[C, F, K],
    ),
    (ALL, "SELECT * FROM orders o WHERE o.total > 10 ‸", &[K]),
    (
        ALL,
        "SELECT * FROM orders o WHERE o.total BETWEEN 1 AND ‸",
        &[C, F, K],
    ),
    (ALL, "SELECT a FROM t GROUP BY ‸", &[C, F, K]),
    (ALL, "SELECT a FROM t GROUP ‸", &[K]),
    (ALL, "SELECT a FROM t GROUP BY a HAVING ‸", &[C, F, K]),
    (ALL, "SELECT a FROM t ORDER BY ‸", &[C, F, K]),
    (ALL, "SELECT a FROM t ORDER BY a ‸", &[K]),
    (ALL, "SELECT a FROM t UNION SELECT ‸ FROM u", &[C, F, K]),
    (ALL, "SELECT a FROM t UNION ‸", &[K]),
    (ALL, "SELECT * FROM (SELECT a FROM t) AS d WHERE d.‸", &[C]),
    (
        PG,
        "SELECT * FROM generate_series(1, 10) AS g WHERE ‸",
        &[C, F, K],
    ),
    (
        MS,
        "SELECT * FROM orders o CROSS APPLY (SELECT ‸) x",
        &[C, F, K],
    ),
    (
        MS,
        "SELECT * FROM orders o WITH (NOLOCK) WHERE ‸",
        &[C, F, K],
    ),
    (MY, "SELECT * FROM `orders` o WHERE o.‸", &[C]),
    (MS, "SELECT * FROM [orders] o WHERE o.‸", &[C]),
    // CTEs.
    (ALL, "WITH c AS (SELECT a FROM t) SELECT c.‸ FROM c", &[C]),
    (ALL, "WITH c AS (SELECT a FROM t) SELECT * FROM ‸", &[R, S]),
    (ALL, "WITH x‸", &[]),
    (ALL, "WITH c AS (SELECT ‸ FROM t) SELECT 1", &[C, F, K]),
    (
        ALL,
        "WITH c (x, y) AS (SELECT 1, 2) SELECT ‸ FROM c",
        &[C, F, K],
    ),
    (
        PG,
        "WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE ‸) SELECT * FROM r",
        &[C, F, K],
    ),
    // INSERT.
    (ALL, "INSERT INTO ‸", &[R, S]),
    (ALL, "INSERT INTO t (‸", &[C]),
    (ALL, "INSERT INTO t (a, ‸) VALUES (1, 2)", &[C]),
    (ALL, "INSERT INTO t (a) VALUES (‸", &[C, F, K]),
    (ALL, "INSERT INTO t (a) SELECT ‸ FROM u", &[C, F, K]),
    (ALL, "INSERT INTO t ‸", &[K]),
    (ALL, "INSERT ‸", &[K]),
    (
        &[Engine::Postgres, Engine::Sqlite],
        "INSERT INTO t (a) VALUES (1) ON CONFLICT (‸",
        &[C],
    ),
    (
        PG,
        "INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET ‸",
        &[C],
    ),
    (
        MY,
        "INSERT INTO t (a) VALUES (1) ON DUPLICATE KEY UPDATE ‸",
        &[C],
    ),
    (MY, "INSERT INTO t SET ‸", &[C]),
    // UPDATE, DELETE, MERGE, TRUNCATE.
    (ALL, "UPDATE ‸", &[R, S]),
    (ALL, "UPDATE t SET ‸", &[C]),
    (ALL, "UPDATE t SET a = 1, ‸", &[C]),
    (ALL, "UPDATE t SET a = ‸", &[C, F, K]),
    (ALL, "UPDATE t SET a = 1 WHERE ‸", &[C, F, K]),
    (ALL, "UPDATE t ‸", &[K]),
    (MS, "UPDATE o SET ‸ FROM orders o", &[C]),
    (PG, "UPDATE t SET a = u.‸ FROM u", &[C]),
    (MY, "UPDATE a JOIN b ON a.id = b.a_id SET a.x = b.‸", &[C]),
    (ALL, "DELETE FROM ‸", &[R, S]),
    (ALL, "DELETE FROM t WHERE ‸", &[C, F, K]),
    (ALL, "DELETE ‸", &[K]),
    (MY, "DELETE a FROM orders a JOIN items i ON ‸", &[C, F, K]),
    (PG, "DELETE FROM t USING u WHERE ‸", &[C, F, K]),
    (MS, "MERGE INTO t USING s ON ‸", &[C, F, K]),
    (
        MS,
        "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET ‸",
        &[C],
    ),
    (NO_LITE, "TRUNCATE ‸", &[R, S]),
    (NO_LITE, "TRUNCATE TABLE ‸", &[R, S]),
    // DDL.
    (ALL, "CREATE TABLE ‸", &[]),
    (ALL, "CREATE TABLE t (‸", &[]),
    (ALL, "CREATE TABLE t (id ‸", &[T]),
    (ALL, "CREATE TABLE t (id int, name ‸", &[T]),
    (ALL, "CREATE TABLE t (id int ‸", &[K]),
    (ALL, "CREATE TABLE t (id int REFERENCES ‸", &[R, S]),
    (ALL, "CREATE TABLE t (id int REFERENCES p (‸", &[C]),
    (ALL, "CREATE ‸", &[K]),
    (ALL, "CREATE VIEW v AS SELECT ‸ FROM t", &[C, F, K]),
    (ALL, "CREATE INDEX i ON ‸", &[R, S]),
    (ALL, "CREATE INDEX i ON t (‸", &[C, F, K]),
    (ALL, "ALTER TABLE ‸", &[R, S]),
    (ALL, "ALTER TABLE t ‸", &[K]),
    (ALL, "ALTER TABLE t DROP COLUMN ‸", &[C]),
    (ALL, "ALTER TABLE t ADD COLUMN c ‸", &[T]),
    (PG, "ALTER TABLE t ALTER COLUMN c TYPE ‸", &[T]),
    (MS, "ALTER TABLE t ALTER COLUMN ‸", &[C]),
    (ALL, "DROP TABLE ‸", &[R, S]),
    (ALL, "DROP TABLE IF EXISTS ‸", &[R, S]),
    (ALL, "DROP ‸", &[K]),
    (PG_MY_MS, "DROP FUNCTION ‸", &[F]),
    (PG_MY_MS, "DROP PROCEDURE ‸", &[P]),
    (PG_MS, "DROP SCHEMA ‸", &[S]),
    // Routines and batches.
    (PG_MY, "CALL ‸", &[P]),
    (MS, "EXEC ‸", &[P]),
    (MS, "EXEC dbo.‸", &[P]),
    (MS, "EXECUTE ‸", &[P]),
    (MY, "USE ‸", &[D, S]),
    (MS, "USE ‸", &[D, S]),
    (
        PG,
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS $$ SELECT ‸ FROM t $$",
        &[C, F, K],
    ),
    (
        PG,
        "CREATE FUNCTION f() RETURNS int AS $$ BEGIN RETURN (SELECT ‸ FROM t); END $$ LANGUAGE plpgsql",
        &[],
    ),
    (PG, "DO $$ BEGIN PERFORM ‸; END $$", &[]),
    (
        MS,
        "CREATE PROCEDURE p @a int AS BEGIN SELECT ‸ FROM t END",
        &[C, F, K],
    ),
    (
        MY,
        "CREATE PROCEDURE p() BEGIN SELECT ‸ FROM t; END",
        &[C, F, K],
    ),
    (
        MY,
        "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT ‸ FROM t; END$$\nDELIMITER ;",
        &[C, F, K],
    ),
    (
        MS,
        "IF EXISTS (SELECT 1 FROM t WHERE ‸) PRINT 'x'",
        &[C, F, K],
    ),
    (MS, "SELECT 1\nGO\n‸", &[K, SN]),
    (MS, "DECLARE @x ‸", &[T]),
    (MS, "SELECT 1\nSELECT * FROM ‸", &[R, S]),
    (
        MS,
        "SET NOCOUNT ON;\nSELECT * FROM orders o WHERE o.‸",
        &[C],
    ),
    (LITE, "SELECT * FROM t INDEXED BY i WHERE ‸", &[C, F, K]),
    // Quoted names being typed.
    (ANSI_QUOTES, "SELECT \"Ord‸", &[C, F]),
    (BACKTICKS, "SELECT `Ord‸", &[C, F]),
    (BRACKETS, "SELECT [Ord‸", &[C, F]),
    (MY, "SELECT \"Ord‸", &[]),
    (ANSI_QUOTES, "SELECT * FROM \"Ord‸", &[R, S]),
];

#[test]
fn context_table() {
    let mut failures = Vec::new();
    let mut checked = 0;
    let mut rows = 0;
    for (engines, text, expected) in TABLE {
        rows += 1;
        for engine in *engines {
            let ctx = at(*engine, text);
            checked += 1;
            let ok = if expected.is_empty() {
                ctx.expected.is_empty()
            } else {
                ctx.expected.starts_with(expected)
            };
            if !ok {
                failures.push(format!(
                    "{engine:?} {text:?}: expected {expected:?}, got {:?} (clause {:?}, class {:?})",
                    ctx.expected, ctx.clause, ctx.class
                ));
            }
        }
    }
    assert!(rows >= 80, "at least 80 cases, got {rows}");
    assert!(checked >= 400, "{checked} checks");
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn qualified_columns_come_from_the_aliased_relation() {
    for engine in ALL {
        let ctx = at(
            *engine,
            "SELECT t.‸ FROM orders t JOIN customers c ON c.id = t.cid",
        );
        assert_eq!(ctx.qualifier.len(), 1);
        assert_eq!(ctx.qualifier[0].text, "t");
        assert_eq!(sources(&ctx), ["orders"], "{engine:?}");
        assert_eq!(scope(&ctx), ["t", "c"], "{engine:?}");
        assert_eq!(ctx.clause, Clause::SelectList);
        assert_eq!(ctx.class, StatementClass::Select);
    }
}

#[test]
fn nested_scopes_list_the_inner_relations_first() {
    for engine in ALL {
        let ctx = at(*engine, "SELECT (SELECT x.‸ FROM t2 x) FROM t1");
        assert_eq!(scope(&ctx), ["x", "t1"], "{engine:?}");
        assert_eq!(sources(&ctx), ["t2"], "{engine:?}");
        // A derived table does not see the relations next to it.
        let ctx = at(*engine, "SELECT * FROM a, (SELECT ‸ FROM b) d");
        assert_eq!(scope(&ctx), ["b"], "{engine:?}");
        // The FROM after the cursor counts.
        let ctx = at(
            *engine,
            "SELECT ‸ FROM orders o JOIN customers c ON c.id = o.cid",
        );
        assert_eq!(scope(&ctx), ["o", "c"], "{engine:?}");
    }
}

#[test]
fn cte_columns_and_names_are_in_scope() {
    for engine in ALL {
        let ctx = at(*engine, "WITH c AS (SELECT a FROM t) SELECT c.‸ FROM c");
        assert_eq!(ctx.expected, [C], "{engine:?}");
        assert_eq!(ctx.ctes.len(), 1);
        assert_eq!(ctx.ctes[0].name.text, "c");
        let columns: Vec<_> = ctx.ctes[0]
            .columns
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(columns, ["a"]);
        assert_eq!(sources(&ctx), ["c"], "{engine:?}");
        assert!(matches!(ctx.scope[0].source, RelationSource::Cte { .. }));

        let ctx = at(*engine, "WITH c (x, y) AS (SELECT 1, 2) SELECT * FROM ‸");
        let columns: Vec<_> = ctx.ctes[0]
            .columns
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(columns, ["x", "y"]);
    }
}

#[test]
fn dml_columns_come_from_the_target_only() {
    for engine in ALL {
        let ctx = at(*engine, "INSERT INTO t (‸) SELECT * FROM u");
        assert_eq!(scope(&ctx), ["t"], "{engine:?}");
        assert_eq!(ctx.clause, Clause::InsertColumns);
        assert_eq!(ctx.class, StatementClass::Insert);
        let ctx = at(*engine, "UPDATE t SET ‸ WHERE x = 1");
        assert_eq!(scope(&ctx), ["t"], "{engine:?}");
        assert_eq!(ctx.clause, Clause::UpdateSet);
        assert_eq!(ctx.class, StatementClass::Update);
        let ctx = at(*engine, "DELETE FROM t WHERE ‸");
        assert_eq!(scope(&ctx), ["t"]);
        assert_eq!(ctx.class, StatementClass::Delete);
    }
    let ctx = at(Engine::Postgres, "UPDATE t SET ‸ FROM u WHERE u.id = t.id");
    assert_eq!(scope(&ctx), ["t"]);
    let ctx = at(
        Engine::Postgres,
        "UPDATE t SET a = ‸ FROM u WHERE u.id = t.id",
    );
    assert_eq!(scope(&ctx), ["t", "u"]);
    let ctx = at(
        Engine::MsSql,
        "UPDATE o SET ‸ FROM orders o JOIN items i ON i.oid = o.id",
    );
    assert_eq!(scope(&ctx), ["o"]);
    assert_eq!(sources(&ctx), ["orders"]);
    let ctx = at(
        Engine::Postgres,
        "CREATE TABLE c (pid int REFERENCES parent (‸",
    );
    assert_eq!(scope(&ctx), ["parent"]);
    let ctx = at(Engine::Postgres, "ALTER TABLE orders DROP COLUMN ‸");
    assert_eq!(scope(&ctx), ["orders"]);
}

#[test]
fn keyword_lists_fit_the_position() {
    for engine in ALL {
        let ctx = at(*engine, "SELECT * FROM orders o ‸");
        assert_eq!(ctx.expected, [K], "{engine:?}");
        for kw in ["WHERE", "JOIN", "GROUP", "ORDER", "LIMIT"] {
            assert!(
                ctx.keywords.contains(&kw),
                "{engine:?} {kw}: {:?}",
                ctx.keywords
            );
        }
        assert!(!ctx.keywords.contains(&"AS"));
        let ctx = at(*engine, "SELECT * FROM orders ‸");
        assert_eq!(ctx.expected, [K]);
        assert!(ctx.keywords.contains(&"AS"));
        let ctx = at(*engine, "‸");
        assert_eq!(ctx.keywords.first(), Some(&"SELECT"));
        assert_eq!(ctx.class, StatementClass::Empty);
        assert_eq!(ctx.clause, Clause::Start);
        let ctx = at(*engine, "SELECT * FROM t WHERE a = 1 ‸");
        assert!(ctx.keywords.contains(&"AND"));
        let ctx = at(*engine, "SELECT a FROM t ORDER BY a ‸");
        assert!(ctx.keywords.contains(&"DESC"));
        // No keywords where no keyword is expected.
        let ctx = at(*engine, "SELECT * FROM ‸");
        assert!(ctx.keywords.is_empty());
    }
}

#[test]
fn word_prefix_and_quote() {
    let ctx = at(Engine::Postgres, "SELECT ord‸er_id FROM t");
    assert_eq!(ctx.word, 7..15);
    assert_eq!(ctx.prefix, "ord");
    assert_eq!(ctx.quote, None);
    let ctx = at(Engine::Postgres, "SELECT ‸ FROM t");
    assert_eq!(ctx.word, 7..7);
    assert_eq!(ctx.prefix, "");
    let ctx = at(Engine::Postgres, "SELECT \"Ord‸");
    assert_eq!(ctx.quote, Some(b'"'));
    assert_eq!(ctx.prefix, "Ord");
    assert_eq!(ctx.word, 7..11);
    assert!(ctx.expects(C));
    assert!(!ctx.expects(K));
    let ctx = at(Engine::MsSql, "SELECT * FROM [My Ta‸");
    assert_eq!(ctx.quote, Some(b'['));
    assert_eq!(ctx.prefix, "My Ta");
    let ctx = at(Engine::MySql, "SELECT `a``b‸` FROM t");
    assert_eq!(ctx.quote, Some(b'`'));
    assert_eq!(ctx.prefix, "a`b");
    let ctx = at(Engine::Postgres, "SELECT s.\"My Tab\".‸");
    let parts: Vec<_> = ctx
        .qualifier
        .iter()
        .map(|p| (p.text.as_str(), p.quoted))
        .collect();
    assert_eq!(parts, [("s", false), ("My Tab", true)]);
    assert_eq!(ctx.expected, [C]);
    // Multi-byte names and an offset inside a character.
    let text = "SELECT * FROM заказы з WHERE з.‸";
    let ctx = at(Engine::Postgres, text);
    assert_eq!(ctx.expected, [C]);
    assert_eq!(scope(&ctx), ["з"]);
    let dialect = Dialect::builtin(Engine::Postgres).unwrap();
    let ctx = context_at(&dialect, "SELECT é", 8);
    assert_eq!(ctx.offset, 7);
    let ctx = context_at(&dialect, "SELECT 1", 999);
    assert_eq!(ctx.offset, 8);
}

#[test]
fn statements_and_classes() {
    let ctx = at(Engine::Postgres, "SELECT 1;\nUPDATE t SET ‸;\nSELECT 2");
    assert_eq!(ctx.statement, 10..22);
    assert_eq!(ctx.class, StatementClass::Update);
    let ctx = at(Engine::Postgres, "SELECT 1;\n\n‸");
    assert_eq!(ctx.statement, 11..11);
    assert_eq!(ctx.class, StatementClass::Empty);
    // A T-SQL batch is one statement; the class is the inner statement's.
    let ctx = at(Engine::MsSql, "SELECT 1\nINSERT INTO t (‸");
    assert_eq!(ctx.statement.start, 0);
    assert_eq!(ctx.class, StatementClass::Insert);
    for (engine, text, class) in [
        (
            Engine::Postgres,
            "CREATE TABLE t (a ‸",
            StatementClass::CreateTable,
        ),
        (
            Engine::Postgres,
            "CREATE VIEW v AS SELECT ‸",
            StatementClass::Select,
        ),
        (
            Engine::Postgres,
            "CREATE INDEX i ON ‸",
            StatementClass::CreateIndex,
        ),
        (Engine::Postgres, "ALTER TABLE ‸", StatementClass::Alter),
        (Engine::Postgres, "DROP TABLE ‸", StatementClass::Drop),
        (Engine::Postgres, "CALL ‸", StatementClass::Call),
        (Engine::MySql, "USE ‸", StatementClass::Use),
        (
            Engine::MsSql,
            "MERGE INTO t USING s ON ‸",
            StatementClass::Merge,
        ),
        (
            Engine::Postgres,
            "EXPLAIN ANALYZE SELECT ‸",
            StatementClass::Select,
        ),
    ] {
        let ctx = at(engine, text);
        if class == StatementClass::Select {
            // The view's query is a statement of its own kind.
            assert!(
                matches!(
                    ctx.class,
                    StatementClass::Select | StatementClass::CreateView
                ),
                "{text}: {:?}",
                ctx.class
            );
        } else {
            assert_eq!(ctx.class, class, "{text}");
        }
    }
}

#[test]
fn clauses_are_reported() {
    for (text, clause) in [
        ("SELECT ‸", Clause::SelectList),
        ("SELECT * FROM ‸", Clause::From),
        ("SELECT * FROM a JOIN ‸", Clause::Join),
        ("SELECT * FROM a JOIN b ON ‸", Clause::JoinOn),
        ("SELECT * FROM a WHERE ‸", Clause::Where),
        ("SELECT a FROM t GROUP BY ‸", Clause::GroupBy),
        ("SELECT a FROM t GROUP BY a HAVING ‸", Clause::Having),
        ("SELECT a FROM t ORDER BY ‸", Clause::OrderBy),
        ("INSERT INTO t (‸", Clause::InsertColumns),
        ("INSERT INTO t VALUES (‸", Clause::Values),
        ("UPDATE t SET ‸", Clause::UpdateSet),
        ("CREATE TABLE t (a ‸", Clause::TypeName),
        ("CALL ‸", Clause::RoutineName),
        ("‸", Clause::Start),
    ] {
        assert_eq!(at(Engine::Postgres, text).clause, clause, "{text}");
    }
}

#[test]
fn outline_is_attached() {
    let ctx = at(
        Engine::Postgres,
        "SELECT o.‸ FROM orders o WHERE o.total > 1",
    );
    assert_eq!(ctx.outline.queries.len(), 1);
    assert_eq!(ctx.outline.queries[0].relations.len(), 1);
    assert!(
        ctx.outline
            .refs
            .iter()
            .any(|r| r.parts.len() == 2 && r.parts[1].text == "total")
    );
}
