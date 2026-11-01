//! Outline tables (`docs/m6-contracts.md` §4.1): statement classes, query blocks, relations
//! with aliases, CTEs, output columns and references with their roles.

use oxbase_insight::{
    Clause, Dialect, NameRef, Outline, RefRole, RelationSource, StatementClass, outline,
};
use oxbase_types::Engine;

const ALL: [Engine; 5] = [
    Engine::Postgres,
    Engine::MySql,
    Engine::MariaDb,
    Engine::Sqlite,
    Engine::MsSql,
];

fn parse(engine: Engine, text: &str) -> Outline {
    let dialect = Dialect::builtin(engine).expect("a SQL engine");
    outline(&dialect, text, 0..text.len())
}

/// The references as `role:a.b.c`.
fn refs(o: &Outline) -> Vec<String> {
    o.refs.iter().map(describe).collect()
}

fn describe(r: &NameRef) -> String {
    let path: Vec<&str> = r.parts.iter().map(|p| p.text.as_str()).collect();
    let role = match r.role {
        RefRole::Relation => "rel",
        RefRole::Column => "col",
        RefRole::Function => "fn",
        RefRole::Procedure => "proc",
        RefRole::Type => "type",
        RefRole::Schema => "schema",
        RefRole::Database => "db",
        RefRole::Definition => "def",
        RefRole::Unknown => "?",
    };
    format!("{role}:{}", path.join("."))
}

/// The relations of block `q` as `name` / `name=alias`.
fn relations(o: &Outline, q: usize) -> Vec<String> {
    o.queries[q]
        .relations
        .iter()
        .map(|r| {
            let name = match &r.source {
                RelationSource::Named(parts) | RelationSource::Function(parts) => parts
                    .iter()
                    .map(|p| p.text.as_str())
                    .collect::<Vec<_>>()
                    .join("."),
                RelationSource::Cte { query, index } => {
                    format!("cte:{}", o.queries[*query].ctes[*index].name.text)
                }
                RelationSource::Derived { .. } => "derived".to_owned(),
                RelationSource::Values { .. } => "values".to_owned(),
            };
            match &r.alias {
                Some(alias) => format!("{name}={}", alias.text),
                None => name,
            }
        })
        .collect()
}

fn output(o: &Outline, q: usize) -> Vec<&str> {
    o.queries[q]
        .output
        .iter()
        .map(|i| i.text.as_str())
        .collect()
}

#[test]
fn statement_classes() {
    let cases = [
        ("SELECT 1", StatementClass::Select),
        (
            "WITH c AS (SELECT 1) SELECT * FROM c",
            StatementClass::Select,
        ),
        ("(SELECT 1) UNION (SELECT 2)", StatementClass::Select),
        ("VALUES (1), (2)", StatementClass::Select),
        ("EXPLAIN SELECT 1", StatementClass::Select),
        ("INSERT INTO t VALUES (1)", StatementClass::Insert),
        (
            "WITH c AS (SELECT 1) INSERT INTO t SELECT * FROM c",
            StatementClass::Insert,
        ),
        ("UPDATE t SET a = 1", StatementClass::Update),
        ("DELETE FROM t", StatementClass::Delete),
        (
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE",
            StatementClass::Merge,
        ),
        ("CREATE TABLE t (a int)", StatementClass::CreateTable),
        ("CREATE VIEW v AS SELECT 1", StatementClass::CreateView),
        (
            "CREATE UNIQUE INDEX i ON t (a)",
            StatementClass::CreateIndex,
        ),
        (
            "CREATE FUNCTION f() RETURNS int AS 'select 1' LANGUAGE sql",
            StatementClass::CreateRoutine,
        ),
        ("CREATE SCHEMA s", StatementClass::CreateOther),
        ("ALTER TABLE t ADD COLUMN b int", StatementClass::Alter),
        ("DROP TABLE t", StatementClass::Drop),
        ("CALL p(1)", StatementClass::Call),
        ("EXEC p 1", StatementClass::Call),
        ("USE db", StatementClass::Use),
        ("SET search_path TO public", StatementClass::Set),
        ("BEGIN", StatementClass::Transaction),
        ("COMMIT", StatementClass::Transaction),
        ("GRANT SELECT ON t TO u", StatementClass::Other),
        ("-- only a comment", StatementClass::Empty),
        ("", StatementClass::Empty),
    ];
    for (text, class) in cases {
        assert_eq!(parse(Engine::Postgres, text).class, class, "{text}");
    }
    // T-SQL: SET prefixes of a batch are skipped.
    assert_eq!(
        parse(Engine::MsSql, "SET NOCOUNT ON\nSELECT 1").class,
        StatementClass::Select
    );
}

#[test]
fn select_blocks_relations_and_refs() {
    for engine in ALL {
        let o = parse(
            engine,
            "SELECT o.id, count(*) AS n FROM sales.orders o JOIN customers AS c ON c.id = o.cid WHERE o.total > 1 GROUP BY o.id ORDER BY n",
        );
        assert_eq!(o.class, StatementClass::Select);
        assert_eq!(o.queries.len(), 1, "{engine:?}");
        assert_eq!(
            relations(&o, 0),
            ["sales.orders=o", "customers=c"],
            "{engine:?}"
        );
        assert_eq!(output(&o, 0), ["id", "n"], "{engine:?}");
        assert_eq!(
            refs(&o),
            [
                "col:o.id",
                "fn:count",
                "def:n",
                "rel:sales.orders",
                "def:o",
                "rel:customers",
                "def:c",
                "col:c.id",
                "col:o.cid",
                "col:o.total",
                "col:o.id",
                "col:n",
            ],
            "{engine:?}"
        );
        let clauses: Vec<Clause> = o.queries[0].clauses.iter().map(|(c, _)| *c).collect();
        assert_eq!(
            clauses,
            [
                Clause::SelectList,
                Clause::From,
                Clause::Join,
                Clause::JoinOn,
                Clause::Where,
                Clause::GroupBy,
                Clause::OrderBy,
            ],
            "{engine:?}"
        );
        let clause_of = |name: &str| {
            o.refs
                .iter()
                .find(|r| r.parts.last().is_some_and(|p| p.text == name))
                .map(|r| r.clause)
        };
        assert_eq!(clause_of("total"), Some(Clause::Where));
        assert_eq!(clause_of("cid"), Some(Clause::JoinOn));
        assert_eq!(o.ref_at(9).map(describe).as_deref(), Some("col:o.id"));
    }
}

#[test]
fn literals_and_parameters_are_not_references() {
    let o = parse(
        Engine::Postgres,
        "SELECT * FROM t WHERE a = $1 AND b = 'x' AND c = :name AND d = ? AND e = 1.5e3 AND f IS NOT NULL AND g = TRUE AND h = DATE '2020-01-01' AND i > now() - INTERVAL '1' DAY",
    );
    assert_eq!(
        refs(&o),
        [
            "rel:t", "col:a", "col:b", "col:c", "col:d", "col:e", "col:f", "col:g", "col:h",
            "col:i", "fn:now",
        ]
    );
    let o = parse(
        Engine::MsSql,
        "SELECT * FROM t WHERE a = @p AND b = N'x' AND #tmp_c = 1",
    );
    assert_eq!(refs(&o), ["rel:t", "col:a", "col:b", "col:#tmp_c"]);
    let o = parse(
        Engine::MySql,
        "SELECT @x := a, @@session.sql_mode FROM t WHERE b = _utf8'x'",
    );
    assert_eq!(refs(&o), ["col:a", "rel:t", "col:b"]);
}

#[test]
fn keyword_forms_inside_calls() {
    let o = parse(
        Engine::Postgres,
        "SELECT CAST(a AS varchar(10)), EXTRACT(YEAR FROM b), SUBSTRING(c FROM 2 FOR 3), TRIM(BOTH ' ' FROM d), string_agg(e, ',' ORDER BY f), x::int FROM t",
    );
    assert_eq!(
        refs(&o),
        [
            "fn:CAST",
            "col:a",
            "type:varchar",
            "fn:EXTRACT",
            "col:b",
            "fn:SUBSTRING",
            "col:c",
            "fn:TRIM",
            "col:d",
            "fn:string_agg",
            "col:e",
            "col:f",
            "col:x",
            "type:int",
            "rel:t",
        ]
    );
    let o = parse(
        Engine::MsSql,
        "SELECT DATEADD(day, 1, a), DATEDIFF(month, b, c) FROM t",
    );
    assert_eq!(
        refs(&o),
        [
            "fn:DATEADD",
            "col:a",
            "fn:DATEDIFF",
            "col:b",
            "col:c",
            "rel:t"
        ]
    );
    // Keywords used as column names stay names.
    let o = parse(Engine::Postgres, "SELECT date, name FROM t");
    assert_eq!(refs(&o), ["col:date", "col:name", "rel:t"]);
}

#[test]
fn case_windows_and_operators() {
    let o = parse(
        Engine::Postgres,
        "SELECT CASE WHEN a > 1 THEN b ELSE c END AS k, sum(d) OVER (PARTITION BY e ORDER BY f DESC) FROM t WHERE g IS DISTINCT FROM h AND i NOT IN (1, 2) AND j BETWEEN 1 AND 2",
    );
    assert_eq!(
        refs(&o),
        [
            "col:a", "col:b", "col:c", "def:k", "fn:sum", "col:d", "col:e", "col:f", "rel:t",
            "col:g", "col:h", "col:i", "col:j",
        ]
    );
    assert_eq!(output(&o, 0), ["k"]);
    let window = o.refs.iter().find(|r| r.parts[0].text == "e").unwrap();
    assert_eq!(window.clause, Clause::Window);
    let order = o.refs.iter().find(|r| r.parts[0].text == "f").unwrap();
    assert_eq!(order.clause, Clause::OrderBy);
}

#[test]
fn ctes_derived_tables_and_subqueries() {
    for engine in ALL {
        let o = parse(
            engine,
            "WITH c (x) AS (SELECT a FROM t), d AS (SELECT b, e AS f FROM u) SELECT * FROM c JOIN d ON d.b = c.x JOIN (SELECT g FROM v) AS w ON w.g = c.x WHERE EXISTS (SELECT 1 FROM z WHERE z.k = c.x)",
        );
        assert_eq!(o.queries[0].ctes.len(), 2, "{engine:?}");
        let cols: Vec<&str> = o.queries[0].ctes[0]
            .columns
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(cols, ["x"]);
        let cols: Vec<&str> = o.queries[0].ctes[1]
            .columns
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(cols, ["b", "f"]);
        assert_eq!(
            relations(&o, 0),
            ["cte:c", "cte:d", "derived=w"],
            "{engine:?}"
        );
        let RelationSource::Derived { query, columns } = &o.queries[0].relations[2].source else {
            panic!("a derived table");
        };
        assert_eq!(o.queries[*query].parent, Some(0));
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].text, "g");
        // The EXISTS subquery is a block whose parent is the main query.
        let z = o
            .queries
            .iter()
            .position(|q| {
                q.relations
                    .iter()
                    .any(|r| matches!(&r.source, RelationSource::Named(p) if p[0].text == "z"))
            })
            .expect("the EXISTS block");
        assert_eq!(o.queries[z].parent, Some(0));
        let names: Vec<String> = o
            .visible_relations(z)
            .iter()
            .map(|r| {
                r.alias
                    .as_ref()
                    .map_or_else(String::new, |a| a.text.clone())
            })
            .collect();
        assert_eq!(names.len(), 4, "{names:?}");
        assert_eq!(o.visible_ctes(z).len(), 2);
        // CTE references are relations, CTE names definitions.
        assert!(refs(&o).contains(&"def:c".to_owned()));
        assert!(refs(&o).contains(&"rel:c".to_owned()));
    }
}

#[test]
fn values_lists_table_functions_and_set_operations() {
    let o = parse(
        Engine::Postgres,
        "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS v (n, s), generate_series(1, 3) AS g, LATERAL (SELECT n) l",
    );
    assert_eq!(
        relations(&o, 0),
        ["values=v", "generate_series=g", "derived=l"]
    );
    let RelationSource::Values { columns } = &o.queries[0].relations[0].source else {
        panic!("a VALUES list");
    };
    assert_eq!(columns.len(), 2);
    let o = parse(
        Engine::Postgres,
        "SELECT a FROM t UNION ALL SELECT b FROM u EXCEPT SELECT c FROM w",
    );
    assert_eq!(o.queries.len(), 3);
    assert!(o.queries.iter().all(|q| q.parent.is_none()));
    assert_eq!(relations(&o, 1), ["u"]);
    assert_eq!(o.query_at(o.text_offset_of("b")), Some(1));
}

trait TextOffset {
    fn text_offset_of(&self, name: &str) -> usize;
}

impl TextOffset for Outline {
    /// The start of the first reference named `name`.
    fn text_offset_of(&self, name: &str) -> usize {
        self.refs
            .iter()
            .find(|r| r.parts.last().is_some_and(|p| p.text == name))
            .map_or(0, |r| r.range().start)
    }
}

#[test]
fn tsql_forms() {
    let o = parse(
        Engine::MsSql,
        "SELECT TOP (5) total = SUM(o.amount), [Customer Name] = c.name FROM dbo.orders AS o WITH (NOLOCK) CROSS APPLY dbo.f(o.id) AS x OUTER APPLY (SELECT 1 AS one) y JOIN [dbo].[customers] c ON c.id = o.cid",
    );
    assert_eq!(output(&o, 0), ["total", "Customer Name"]);
    assert_eq!(
        relations(&o, 0),
        ["dbo.orders=o", "dbo.f=x", "derived=y", "dbo.customers=c"]
    );
    let quoted = o
        .refs
        .iter()
        .find(|r| {
            r.role == RefRole::Relation && r.parts.len() == 2 && r.parts[1].text == "customers"
        })
        .expect("the bracketed relation");
    assert!(quoted.parts.iter().all(|p| p.quoted));
    // A batch: every statement's references, the first statement's class.
    let o = parse(
        Engine::MsSql,
        "DECLARE @n int = 1\nIF @n > 0\nBEGIN\n  UPDATE t SET a = 1 WHERE b = @n\nEND\nSELECT c FROM u",
    );
    assert_eq!(o.class, StatementClass::Other);
    assert_eq!(
        refs(&o),
        ["type:int", "rel:t", "col:a", "col:b", "col:c", "rel:u"]
    );
    let o = parse(
        Engine::MsSql,
        "UPDATE o SET o.x = 1 FROM dbo.orders o WHERE o.id = 2",
    );
    assert_eq!(relations(&o, 0), ["dbo.orders=o"]);
    let o = parse(Engine::MsSql, "EXEC @rc = dbo.p @a = 1, @b = 'x' OUTPUT");
    assert_eq!(refs(&o), ["proc:dbo.p"]);
}

#[test]
fn mysql_forms() {
    for engine in [Engine::MySql, Engine::MariaDb] {
        let o = parse(
            engine,
            "UPDATE a JOIN b ON a.id = b.a_id SET a.x = b.y, a.z = 1 WHERE b.k = 2",
        );
        assert_eq!(relations(&o, 0), ["a", "b"]);
        let o = parse(
            engine,
            "DELETE a FROM orders a JOIN items i ON i.oid = a.id WHERE i.n = 0",
        );
        assert_eq!(relations(&o, 0), ["orders=a", "items=i"]);
        let o = parse(
            engine,
            "INSERT INTO t (a, b) VALUES (1, 2) ON DUPLICATE KEY UPDATE b = VALUES(b) + 1",
        );
        assert_eq!(
            refs(&o),
            ["rel:t", "col:a", "col:b", "col:b", "fn:VALUES", "col:b"]
        );
        let o = parse(
            engine,
            "SELECT `select`, `a b` FROM `my db`.`t` USE INDEX (i) WHERE 1",
        );
        assert_eq!(refs(&o), ["col:select", "col:a b", "rel:my db.t"]);
        assert_eq!(relations(&o, 0), ["my db.t"]);
    }
}

#[test]
fn dml_forms() {
    for engine in ALL {
        let o = parse(
            engine,
            "INSERT INTO s.t (a, b) SELECT x, y FROM u WHERE z = 1",
        );
        assert_eq!(o.class, StatementClass::Insert);
        assert_eq!(relations(&o, 0), ["s.t"]);
        assert_eq!(o.queries.len(), 2);
        assert_eq!(relations(&o, 1), ["u"]);
        assert_eq!(
            refs(&o),
            [
                "rel:s.t", "col:a", "col:b", "col:x", "col:y", "rel:u", "col:z"
            ]
        );
        let insert_columns = o.refs.iter().find(|r| r.parts[0].text == "a").unwrap();
        assert_eq!(insert_columns.clause, Clause::InsertColumns);
        let o = parse(engine, "DELETE FROM t WHERE a = 1");
        assert_eq!(relations(&o, 0), ["t"]);
        let o = parse(engine, "UPDATE t SET a = b + 1 WHERE c = 2");
        assert_eq!(refs(&o), ["rel:t", "col:a", "col:b", "col:c"]);
        let set = o.refs.iter().find(|r| r.parts[0].text == "a").unwrap();
        assert_eq!(set.clause, Clause::UpdateSet);
    }
    let o = parse(
        Engine::Postgres,
        "INSERT INTO t AS x (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET a = excluded.a WHERE x.a > 0 RETURNING x.a, b",
    );
    assert_eq!(relations(&o, 0), ["t=x"]);
    assert_eq!(output(&o, 0), ["a", "b"]);
    let o = parse(
        Engine::Postgres,
        "DELETE FROM t USING u WHERE t.id = u.id RETURNING *",
    );
    assert_eq!(relations(&o, 0), ["t", "u"]);
    assert!(o.queries[0].star);
    let o = parse(
        Engine::MsSql,
        "MERGE INTO t AS d USING s ON d.id = s.id WHEN MATCHED AND s.x = 1 THEN UPDATE SET d.v = s.v WHEN NOT MATCHED BY TARGET THEN INSERT (id, v) VALUES (s.id, s.v);",
    );
    assert_eq!(o.class, StatementClass::Merge);
    assert_eq!(relations(&o, 0), ["t=d", "s"]);
    let o = parse(
        Engine::Postgres,
        "WITH moved AS (DELETE FROM a WHERE x = 1 RETURNING *) INSERT INTO b SELECT * FROM moved",
    );
    assert_eq!(o.class, StatementClass::Insert);
    assert_eq!(relations(&o, 0), ["b"]);
}

#[test]
fn ddl_forms() {
    let o = parse(
        Engine::Postgres,
        "CREATE TABLE IF NOT EXISTS s.t (id bigint PRIMARY KEY, name varchar(10) NOT NULL DEFAULT 'x', p int REFERENCES parent (id), CONSTRAINT c CHECK (id > 0), FOREIGN KEY (p) REFERENCES other (k))",
    );
    assert_eq!(o.class, StatementClass::CreateTable);
    assert_eq!(
        refs(&o),
        [
            "def:s.t",
            "def:id",
            "type:bigint",
            "def:name",
            "type:varchar",
            "def:p",
            "type:int",
            "rel:parent",
            "col:id",
            "col:id",
            "rel:other",
            "col:k",
        ]
    );
    let o = parse(
        Engine::Postgres,
        "CREATE INDEX CONCURRENTLY i ON t USING btree (a, lower(b)) WHERE c",
    );
    assert_eq!(relations(&o, 0), ["t"]);
    assert_eq!(
        refs(&o),
        ["def:i", "rel:t", "col:a", "fn:lower", "col:b", "col:c"]
    );
    let o = parse(
        Engine::Postgres,
        "ALTER TABLE ONLY t DROP COLUMN a, ADD COLUMN b text, RENAME COLUMN c TO d",
    );
    assert_eq!(relations(&o, 0), ["t"]);
    assert_eq!(refs(&o), ["rel:t", "col:a", "def:b", "type:text", "col:c"]);
    let o = parse(Engine::Postgres, "DROP TABLE IF EXISTS a, s.b CASCADE");
    assert_eq!(refs(&o), ["rel:a", "rel:s.b"]);
    let o = parse(Engine::Postgres, "CREATE VIEW v (x) AS SELECT a FROM t");
    assert_eq!(o.class, StatementClass::CreateView);
    assert_eq!(refs(&o), ["def:v", "def:x", "col:a", "rel:t"]);
    let o = parse(Engine::MySql, "USE shop");
    assert_eq!(refs(&o), ["db:shop"]);
    let o = parse(Engine::Postgres, "CALL s.p(a, 2)");
    assert_eq!(refs(&o), ["proc:s.p", "col:a"]);
    let o = parse(Engine::Postgres, "TRUNCATE TABLE a, b");
    assert_eq!(refs(&o), ["rel:a", "rel:b"]);
}

#[test]
fn routine_bodies_are_statements() {
    let o = parse(
        Engine::MySql,
        "CREATE PROCEDURE p(IN a INT) BEGIN DECLARE n INT DEFAULT 0; SELECT x INTO n FROM t WHERE y = a; END",
    );
    assert_eq!(o.class, StatementClass::CreateRoutine);
    assert!(refs(&o).contains(&"rel:t".to_owned()), "{:?}", refs(&o));
    let o = parse(
        Engine::MsSql,
        "CREATE OR ALTER PROCEDURE dbo.p @a int, @b varchar(10) = NULL OUTPUT AS BEGIN SET NOCOUNT ON; SELECT c FROM t WHERE d = @a END",
    );
    assert_eq!(o.class, StatementClass::CreateRoutine);
    assert!(refs(&o).contains(&"rel:t".to_owned()), "{:?}", refs(&o));
    let o = parse(
        Engine::Postgres,
        "CREATE FUNCTION f(a int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN a; END $$",
    );
    assert_eq!(o.class, StatementClass::CreateRoutine);
    assert_eq!(refs(&o), ["def:f", "type:int", "type:int"]);
    let o = parse(
        Engine::Sqlite,
        "CREATE TRIGGER tr AFTER INSERT ON t FOR EACH ROW BEGIN UPDATE u SET n = n + 1; END",
    );
    assert!(refs(&o).contains(&"rel:t".to_owned()));
    assert!(refs(&o).contains(&"rel:u".to_owned()));
}

#[test]
fn quoted_identifiers_keep_their_quoting() {
    let o = parse(
        Engine::Postgres,
        "SELECT \"My Col\", plain FROM \"My Table\" \"T\"",
    );
    let quoted: Vec<(String, bool)> = o
        .refs
        .iter()
        .flat_map(|r| r.parts.iter().map(|p| (p.text.clone(), p.quoted)))
        .collect();
    assert_eq!(
        quoted,
        [
            ("My Col".to_owned(), true),
            ("plain".to_owned(), false),
            ("My Table".to_owned(), true),
            ("T".to_owned(), true),
        ]
    );
}

#[test]
fn incomplete_statements_give_partial_outlines() {
    let o = parse(Engine::Postgres, "SELECT a, FROM t WHERE");
    assert_eq!(o.class, StatementClass::Select);
    assert_eq!(relations(&o, 0), ["t"]);
    let o = parse(Engine::Postgres, "SELECT t. FROM orders t");
    assert_eq!(refs(&o), ["rel:orders", "def:t"]);
    let o = parse(Engine::Postgres, "SELECT * FROM (SELECT a FROM");
    assert_eq!(o.queries.len(), 2);
    let o = parse(Engine::Postgres, "INSERT INTO");
    assert_eq!(o.class, StatementClass::Insert);
    // An out-of-range or split range never panics.
    let dialect = Dialect::builtin(Engine::Postgres).unwrap();
    let e = outline(&dialect, "SELECT é", 8..100);
    assert_eq!(e.class, StatementClass::Empty);
    #[allow(clippy::reversed_empty_ranges)] // a reversed range on purpose
    let e = outline(&dialect, "SELECT 1", 5..2);
    assert_eq!(e.class, StatementClass::Empty);
}

#[test]
fn ordinality_and_rows_from_keep_the_statement_whole() {
    let o = parse(
        Engine::Postgres,
        "SELECT * FROM unnest(a) WITH ORDINALITY AS u(x, n) WHERE x > 1 ORDER BY n",
    );
    assert_eq!(o.class, StatementClass::Select);
    assert_eq!(o.queries.len(), 1, "{:?}", o.queries);
    assert!(
        o.queries.iter().all(|q| q.ctes.is_empty()),
        "no phantom CTE"
    );
    assert_eq!(relations(&o, 0), ["unnest=u"]);
    let aliases: Vec<&str> = o.queries[0].relations[0]
        .column_aliases
        .iter()
        .map(|c| c.text.as_str())
        .collect();
    assert_eq!(aliases, ["x", "n"]);
    let clauses: Vec<Clause> = o.queries[0].clauses.iter().map(|(c, _)| *c).collect();
    assert!(clauses.contains(&Clause::Where), "{clauses:?}");
    assert!(clauses.contains(&Clause::OrderBy), "{clauses:?}");
    assert!(refs(&o).contains(&"col:x".to_owned()));

    let o = parse(
        Engine::Postgres,
        "SELECT * FROM ROWS FROM (generate_series(1, 2), unnest(b)) WITH ORDINALITY AS r(a, b, n) WHERE a > 0",
    );
    assert_eq!(o.queries.len(), 1);
    assert_eq!(relations(&o, 0), ["=r"]);
    let all = refs(&o);
    assert!(all.contains(&"fn:generate_series".to_owned()), "{all:?}");
    assert!(all.contains(&"fn:unnest".to_owned()), "{all:?}");
    assert!(all.contains(&"col:a".to_owned()), "{all:?}");
}

#[test]
fn visible_helpers_follow_isolation_and_cte_owners() {
    for engine in ALL {
        // A later set-operation branch sees the CTEs of the WITH before the first branch.
        let o = parse(
            engine,
            "WITH c AS (SELECT 1 AS a) SELECT a FROM t UNION ALL SELECT a FROM c",
        );
        let branch = o
            .queries
            .iter()
            .position(|q| {
                q.relations
                    .iter()
                    .any(|r| matches!(r.source, RelationSource::Cte { .. }))
            })
            .expect("the branch reading c");
        assert_ne!(branch, 0, "{engine:?}");
        let ctes: Vec<&str> = o
            .visible_ctes(branch)
            .iter()
            .map(|(_, c)| c.name.text.as_str())
            .collect();
        assert_eq!(ctes, ["c"], "{engine:?}");
        assert_eq!(o.queries[branch].cte_parent, Some(0));
        assert_eq!(o.visible_relations(branch).len(), 1, "{engine:?}");

        // A derived table does not see the relations next to it.
        let o = parse(
            engine,
            "SELECT * FROM t1 JOIN (SELECT id FROM t2) d ON d.id = t1.id",
        );
        let RelationSource::Derived { query, .. } = &o.queries[0].relations[1].source else {
            panic!("a derived table");
        };
        assert!(o.queries[*query].isolated);
        assert!(!o.queries[0].isolated);
        let names: Vec<String> = o
            .visible_relations(*query)
            .iter()
            .map(|r| match &r.source {
                RelationSource::Named(p) => p[0].text.clone(),
                _ => "?".to_owned(),
            })
            .collect();
        assert_eq!(names, ["t2"], "{engine:?}");

        // ... but sees an enclosing query's (correlation from a scalar subquery).
        let o = parse(
            engine,
            "SELECT (SELECT 1 FROM (SELECT x FROM t3) d2) FROM t1",
        );
        let derived = o
            .queries
            .iter()
            .position(|q| q.isolated)
            .expect("the derived block");
        let names: Vec<String> = o
            .visible_relations(derived)
            .iter()
            .map(|r| match &r.source {
                RelationSource::Named(p) => p[0].text.clone(),
                _ => "?".to_owned(),
            })
            .collect();
        assert_eq!(names, ["t3", "t1"], "{engine:?}");
    }
}

#[test]
fn compact_literals_and_batch_boundaries() {
    for engine in ALL {
        let o = parse(
            engine,
            "SELECT CASE WHEN a THEN'x' ELSE'y' END AS k, b FROM t WHERE c LIKE'%a'",
        );
        assert_eq!(output(&o, 0), ["k", "b"], "{engine:?}");
        assert_eq!(relations(&o, 0), ["t"], "{engine:?}");
        assert!(refs(&o).contains(&"col:c".to_owned()), "{engine:?}");
    }
    // T-SQL: THROW / RAISERROR start statements, never aliases.
    let o = parse(Engine::MsSql, "SELECT a FROM t\nTHROW 50000, 'x', 1");
    assert_eq!(relations(&o, 0), ["t"]);
    let o = parse(Engine::MsSql, "SELECT a\nRAISERROR('x', 16, 1)");
    assert_eq!(output(&o, 0), ["a"]);
    // A GRANT does not swallow the next statement of the batch.
    let o = parse(
        Engine::MsSql,
        "GRANT SELECT, INSERT ON t TO u WITH GRANT OPTION\nSELECT * FROM a JOIN b ON b.id = a.id",
    );
    assert!(refs(&o).contains(&"rel:t".to_owned()));
    assert_eq!(o.queries.len(), 1);
    assert_eq!(relations(&o, 0), ["a", "b"]);
    let o = parse(Engine::MsSql, "GRANT CONNECT TO u\nSELECT * FROM a");
    assert_eq!(relations(&o, 0), ["a"]);
    // A trigger header without ON ends at the next statement.
    let o = parse(
        Engine::MsSql,
        "CREATE TRIGGER trg\nSELECT * FROM a JOIN b ON 1 = 1",
    );
    assert_eq!(relations(&o, 0), ["a", "b"]);
    let o = parse(
        Engine::MsSql,
        "CREATE TRIGGER trg ON t WITH ENCRYPTION AFTER INSERT, UPDATE AS SELECT * FROM inserted",
    );
    assert_eq!(o.class, StatementClass::CreateRoutine);
    assert_eq!(relations(&o, 0), ["inserted"]);
}
