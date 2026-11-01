//! Diagnostics policy (`docs/m6-contracts.md` §4.5) and the valid-SQL corpus (§9): every
//! statement of `tests/corpus/*.sql` must give no error and no warning, with and without a
//! matching schema cache; targeted cases check each finding, its range and its severity.

use std::path::Path;
use std::time::Instant;

use oxbase_insight::{
    CancellationToken, Diagnostic, DiagnosticCode, Dialect, SchemaCache, Severity, SqlLanguage,
    diagnose,
};
use oxbase_types::{
    CatalogColumn, CatalogForeignKey, CatalogRelation, CatalogRoutine, CatalogSchema,
    CatalogSnapshot, Engine, ErrorCategory, RelationKind, RoutineKind, ServerVersion,
};

use DiagnosticCode::{
    AmbiguousColumn, DialectMismatch, MissingIfExists, MissingWhere, SyntaxError,
    UnbalancedParenthesis, UnknownColumn, UnknownFunction, UnknownRelation, UnknownSchema,
    UnterminatedComment, UnterminatedIdentifier, UnterminatedString, WrongArgumentCount,
};
use Severity::{Error, Hint, Warning};

// --- the sample schema the corpus is written against -----------------------------------------

const TABLES: &[(&str, &[&str])] = &[
    (
        "customers",
        &[
            "id",
            "name",
            "email",
            "country",
            "city",
            "created_at",
            "status",
            "credit_limit",
        ],
    ),
    (
        "orders",
        &[
            "id",
            "customer_id",
            "order_date",
            "status",
            "total",
            "shipped_at",
            "notes",
        ],
    ),
    (
        "order_items",
        &[
            "id",
            "order_id",
            "product_id",
            "quantity",
            "unit_price",
            "discount",
        ],
    ),
    (
        "products",
        &[
            "id",
            "sku",
            "name",
            "category_id",
            "price",
            "stock",
            "discontinued",
            "created_at",
        ],
    ),
    ("categories", &["id", "name", "parent_id"]),
    (
        "employees",
        &[
            "id",
            "first_name",
            "last_name",
            "email",
            "manager_id",
            "department_id",
            "salary",
            "hired_at",
            "active",
        ],
    ),
    ("departments", &["id", "name", "budget", "location"]),
    (
        "audit_log",
        &[
            "id",
            "table_name",
            "action",
            "changed_at",
            "changed_by",
            "payload",
        ],
    ),
    (
        "invoices",
        &[
            "id",
            "order_id",
            "amount",
            "issued_at",
            "paid_at",
            "due_date",
        ],
    ),
    (
        "payments",
        &["id", "invoice_id", "amount", "paid_at", "method"],
    ),
];

fn column(name: &str) -> CatalogColumn {
    let key = name == "id" || name.ends_with("_id");
    let native_type = if key { "integer" } else { "text" };
    CatalogColumn {
        name: name.to_owned(),
        native_type: native_type.to_owned(),
        nullable: name != "id",
        primary_key: name == "id",
    }
}

fn relation(name: &str, kind: RelationKind, columns: &[&str]) -> CatalogRelation {
    CatalogRelation {
        name: name.to_owned(),
        kind,
        columns: columns.iter().copied().map(column).collect(),
        foreign_keys: Vec::new(),
        comment: None,
    }
}

fn routine(name: &str, kind: RoutineKind) -> CatalogRoutine {
    CatalogRoutine {
        name: name.to_owned(),
        kind,
        args: "order_id integer".to_owned(),
        returns: (kind != RoutineKind::Procedure).then(|| "numeric".to_owned()),
    }
}

/// The catalog the corpus uses: database `shop` (SQLite: `main`), the tables above in the
/// default schema, a view, three routines, and on PostgreSQL / SQL Server a `sales` schema.
fn sample_snapshot(engine: Engine) -> CatalogSnapshot {
    let (database, main, with_sales) = match engine {
        Engine::Postgres => ("shop", "public", true),
        Engine::MsSql => ("shop", "dbo", true),
        Engine::Sqlite => ("main", "main", false),
        _ => ("shop", "shop", false),
    };
    let mut snap = CatalogSnapshot::new(engine, ServerVersion::new(16, 0, 0), database);
    snap.databases = vec![database.to_owned(), "archive".to_owned()];
    snap.default_schema = Some(main.to_owned());
    snap.search_path = vec![main.to_owned()];
    let mut schema = CatalogSchema::new(main);
    for (name, columns) in TABLES {
        let mut rel = relation(name, RelationKind::Table, columns);
        if *name == "orders" {
            rel.foreign_keys.push(CatalogForeignKey {
                columns: vec!["customer_id".to_owned()],
                ref_schema: None,
                ref_table: "customers".to_owned(),
                ref_columns: vec!["id".to_owned()],
            });
        }
        schema.relations.push(rel);
    }
    schema.relations.push(relation(
        "v_customer_totals",
        RelationKind::View,
        &["customer_id", "name", "order_count", "total_amount"],
    ));
    schema.routines = vec![
        routine("fn_order_total", RoutineKind::Function),
        routine("order_total", RoutineKind::Function),
        routine("usp_archive_orders", RoutineKind::Procedure),
    ];
    schema.sequences = vec!["invoice_numbers".to_owned()];
    snap.schemas.push(schema);
    if with_sales {
        let mut sales = CatalogSchema::new("sales");
        sales.relations = vec![
            relation(
                "regions",
                RelationKind::Table,
                &["id", "name", "manager_id"],
            ),
            relation(
                "targets",
                RelationKind::Table,
                &["id", "region_id", "year", "amount"],
            ),
        ];
        snap.schemas.push(sales);
    }
    snap
}

fn sample(engine: Engine) -> SchemaCache {
    SchemaCache::new(sample_snapshot(engine))
}

// --- helpers ------------------------------------------------------------------------------------

fn run(engine: Engine, sql: &str, cache: Option<&SchemaCache>) -> Vec<Diagnostic> {
    let dialect = Dialect::builtin(engine).unwrap();
    diagnose(&dialect, sql, cache, &CancellationToken::new()).unwrap()
}

type Found = Vec<(DiagnosticCode, Severity, String)>;

/// (code, severity, underlined text) of every finding.
fn found(engine: Engine, sql: &str, cache: Option<&SchemaCache>) -> Found {
    run(engine, sql, cache)
        .into_iter()
        .map(|d| (d.code, d.severity, sql[d.range].to_owned()))
        .collect()
}

#[track_caller]
fn assert_has(f: &Found, code: DiagnosticCode, severity: Severity, text: &str) {
    assert!(
        f.contains(&(code, severity, text.to_owned())),
        "expected {code:?} {severity:?} on `{text}` in {f:?}"
    );
}

#[track_caller]
fn assert_lacks(f: &Found, code: DiagnosticCode) {
    assert!(
        f.iter().all(|(c, _, _)| *c != code),
        "unexpected {code:?} in {f:?}"
    );
}

/// Findings that are warnings or errors.
fn loud(f: &Found) -> Found {
    f.iter()
        .filter(|(_, s, _)| *s >= Warning)
        .cloned()
        .collect()
}

fn corpus(file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

const CORPUS: &[(&str, &[Engine])] = &[
    ("postgres.sql", &[Engine::Postgres]),
    ("mysql.sql", &[Engine::MySql, Engine::MariaDb]),
    ("sqlite.sql", &[Engine::Sqlite]),
    ("mssql.sql", &[Engine::MsSql]),
];

// --- the valid corpus -----------------------------------------------------------------------

#[test]
fn the_valid_corpus_has_no_errors_or_warnings() {
    let mut problems = Vec::new();
    for (file, engines) in CORPUS {
        let text = corpus(file);
        for &engine in *engines {
            let dialect = Dialect::builtin(engine).unwrap();
            let statements = oxbase_sql::split(dialect.sql, &text).len();
            assert!(statements >= 150, "{file}: {statements} statements");
            let cache = sample(engine);
            for (label, cache) in [("no cache", None), ("sample cache", Some(&cache))] {
                let diags = diagnose(&dialect, &text, cache, &CancellationToken::new()).unwrap();
                for d in diags.iter().filter(|d| d.severity >= Warning) {
                    let line = text[..d.range.start].matches('\n').count() + 1;
                    problems.push(format!(
                        "{file}:{line} ({engine:?}, {label}) {} {:?} `{}`: {}",
                        d.code,
                        d.severity,
                        &text[d.range.clone()],
                        d.message
                    ));
                }
            }
        }
    }
    assert_eq!(problems.join("\n"), "");
}

#[test]
fn every_prefix_of_the_corpus_is_diagnosed_without_panicking() {
    for (file, engines) in CORPUS {
        let text = corpus(file);
        let head: String = text.chars().take(2500).collect();
        let cache = sample(engines[0]);
        let dialect = Dialect::builtin(engines[0]).unwrap();
        let token = CancellationToken::new();
        for (i, _) in head.char_indices().step_by(3) {
            let prefix = &head[..i];
            diagnose(&dialect, prefix, Some(&cache), &token).unwrap();
            diagnose(&dialect, prefix, None, &token).unwrap();
        }
    }
}

// --- lexical findings -------------------------------------------------------------------------

#[test]
fn unterminated_tokens_are_errors_on_their_first_line() {
    let f = found(Engine::Postgres, "SELECT 'abc\ndef", None);
    assert_eq!(f, vec![(UnterminatedString, Error, "'abc".to_owned())]);
    let f = found(Engine::Postgres, "SELECT \"abc", None);
    assert_eq!(f, vec![(UnterminatedIdentifier, Error, "\"abc".to_owned())]);
    let f = found(Engine::Postgres, "SELECT 1 /* open", None);
    assert_eq!(f, vec![(UnterminatedComment, Error, "/* open".to_owned())]);
    let f = found(Engine::Postgres, "SELECT $$ body", None);
    assert_eq!(f, vec![(UnterminatedString, Error, "$$ body".to_owned())]);
    let f = found(Engine::MySql, "SELECT `abc", None);
    assert_eq!(f, vec![(UnterminatedIdentifier, Error, "`abc".to_owned())]);
    let f = found(Engine::MsSql, "SELECT [abc", None);
    assert_eq!(f, vec![(UnterminatedIdentifier, Error, "[abc".to_owned())]);
    // A comment after the last statement is no statement, but still checked.
    let f = found(Engine::Sqlite, "SELECT 1; /* open", None);
    assert_eq!(f, vec![(UnterminatedComment, Error, "/* open".to_owned())]);
}

#[test]
fn parentheses_must_balance_within_a_statement() {
    let f = found(Engine::Postgres, "SELECT (1 + 2", None);
    assert_eq!(f, vec![(UnbalancedParenthesis, Error, "(".to_owned())]);
    let f = found(Engine::Postgres, "SELECT 1)", None);
    assert_eq!(f, vec![(UnbalancedParenthesis, Error, ")".to_owned())]);
    let f = found(Engine::Postgres, "SELECT '(', \")\" FROM t -- (", None);
    assert_lacks(&f, UnbalancedParenthesis);
    let f = found(Engine::Postgres, "SELECT (1; SELECT 2)", None);
    assert_eq!(
        f.iter().filter(|x| x.0 == UnbalancedParenthesis).count(),
        2,
        "{f:?}"
    );
    // The string that runs to the end is the one error.
    let f = found(Engine::Postgres, "SELECT f('abc", None);
    assert_eq!(f, vec![(UnterminatedString, Error, "'abc".to_owned())]);
}

// --- dangerous statements ---------------------------------------------------------------------

#[test]
fn update_and_delete_without_where_are_warnings() {
    let f = found(Engine::Postgres, "UPDATE orders SET total = 0", None);
    assert_has(&f, MissingWhere, Warning, "UPDATE orders");
    let f = found(Engine::Postgres, "DELETE FROM orders", None);
    assert_has(&f, MissingWhere, Warning, "DELETE FROM orders");
    let f = found(Engine::MsSql, "DELETE orders", None);
    assert_has(&f, MissingWhere, Warning, "DELETE orders");
    let f = found(
        Engine::MsSql,
        "SELECT 1\nUPDATE orders SET total = 0\nGO",
        None,
    );
    assert_has(&f, MissingWhere, Warning, "UPDATE orders");
    for ok in [
        "DELETE FROM orders WHERE id = 1",
        "UPDATE orders SET total = 0 WHERE id = 1",
        "TRUNCATE TABLE orders",
        "UPDATE orders o JOIN customers c ON c.id = o.customer_id SET o.total = 0",
        "DELETE FROM orders WHERE CURRENT OF c",
    ] {
        assert_lacks(&found(Engine::MySql, ok, None), MissingWhere);
    }
}

#[test]
fn drop_without_if_exists_is_a_hint() {
    let f = found(Engine::Postgres, "DROP TABLE orders", None);
    assert_eq!(f, vec![(MissingIfExists, Hint, "DROP TABLE".to_owned())]);
    let f = found(Engine::Postgres, "DROP VIEW v_customer_totals", None);
    assert_has(&f, MissingIfExists, Hint, "DROP VIEW");
    assert_lacks(
        &found(Engine::Postgres, "DROP TABLE IF EXISTS orders", None),
        MissingIfExists,
    );
    // An unknown table in a DROP is only a hint (`IF OBJECT_ID(...) IS NOT NULL DROP ...`).
    let cache = sample(Engine::MsSql);
    let f = found(
        Engine::MsSql,
        "IF OBJECT_ID('dbo.gone') IS NOT NULL DROP TABLE dbo.gone",
        Some(&cache),
    );
    assert_has(&f, UnknownRelation, Hint, "gone");
}

// --- dialect mismatches -------------------------------------------------------------------------

#[test]
fn syntax_of_another_dialect_is_a_warning() {
    let f = found(
        Engine::MsSql,
        "SELECT id FROM orders ORDER BY id LIMIT 10",
        None,
    );
    assert_has(&f, DialectMismatch, Warning, "LIMIT");
    let f = found(Engine::Postgres, "SELECT TOP 10 id FROM orders", None);
    assert_has(&f, DialectMismatch, Warning, "TOP 10");
    let f = found(Engine::Postgres, "SELECT TOP (10) id FROM orders", None);
    assert_has(&f, DialectMismatch, Hint, "TOP (");
    let f = found(
        Engine::MySql,
        "SELECT id FROM customers WHERE name ILIKE 'a%'",
        None,
    );
    assert_has(&f, DialectMismatch, Warning, "ILIKE");
    let f = found(Engine::Sqlite, "SELECT id::text FROM orders", None);
    assert_has(&f, DialectMismatch, Warning, "::");
    let f = found(Engine::Postgres, "SELECT `id` FROM orders", None);
    assert_has(&f, DialectMismatch, Warning, "`id`");
    let f = found(Engine::Postgres, "SELECT id FROM orders LIMIT 5, 10", None);
    assert_has(&f, DialectMismatch, Warning, "LIMIT 5,");
    let f = found(Engine::Postgres, "SELECT 1\nGO\nSELECT 2", None);
    assert_has(&f, DialectMismatch, Warning, "GO");
    let f = found(Engine::Sqlite, "DELIMITER //\nSELECT 1//", None);
    assert_has(&f, DialectMismatch, Warning, "DELIMITER");
    let f = found(
        Engine::MySql,
        "DELETE FROM orders WHERE id = 1 RETURNING id",
        None,
    );
    assert_has(&f, DialectMismatch, Hint, "RETURNING");
    // The same syntax in its own dialect is fine.
    for (engine, sql) in [
        (Engine::MsSql, "SELECT TOP 10 id FROM orders"),
        (Engine::MsSql, "SELECT TOP (10) id FROM orders"),
        (Engine::Postgres, "SELECT id FROM orders LIMIT 10"),
        (
            Engine::Postgres,
            "SELECT id::text FROM orders WHERE name ILIKE 'a%'",
        ),
        (Engine::MySql, "SELECT `id` FROM orders LIMIT 5, 10"),
        (
            Engine::MariaDb,
            "DELETE FROM orders WHERE id = 1 RETURNING id",
        ),
        (Engine::MsSql, "SELECT limit FROM orders"),
        (Engine::Postgres, "SELECT top FROM orders"),
    ] {
        assert_lacks(&found(engine, sql, None), DialectMismatch);
    }
}

// --- syntax ---------------------------------------------------------------------------------------

#[test]
fn certain_syntax_errors_are_warnings_and_incomplete_clauses_hints() {
    let f = found(Engine::Postgres, "SELECT a, FROM orders", None);
    assert_has(&f, SyntaxError, Warning, ",");
    let f = found(Engine::Postgres, "SELECT a,, b FROM orders", None);
    assert_has(&f, SyntaxError, Warning, ",,");
    let f = found(Engine::Postgres, "SELECT f(1,) FROM orders", None);
    assert_has(&f, SyntaxError, Warning, ",)");
    let f = found(
        Engine::Postgres,
        "SELECT id FROM orders GROUP BY id WHERE id = 1",
        None,
    );
    assert_has(&f, SyntaxError, Warning, "WHERE");
    let f = found(
        Engine::MySql,
        "SELECT id FROM orders ORDER BY id WHERE id = 1",
        None,
    );
    assert_has(&f, SyntaxError, Warning, "WHERE");
    let f = found(Engine::Postgres, "SELECT * FROM orders WHERE", None);
    assert_eq!(f, vec![(SyntaxError, Hint, "WHERE".to_owned())]);
    let f = found(Engine::Postgres, "SELECT * FROM orders ORDER BY", None);
    assert_eq!(f, vec![(SyntaxError, Hint, "ORDER BY".to_owned())]);
    let f = found(Engine::Postgres, "SELECT a,", None);
    assert_eq!(f, vec![(SyntaxError, Hint, ",".to_owned())]);
    // Valid orders and lists.
    for sql in [
        "SELECT id FROM orders WHERE id > 1 GROUP BY id HAVING count(*) > 1 ORDER BY id LIMIT 1 OFFSET 2",
        "SELECT id FROM orders LIMIT 1 OFFSET 2 FOR UPDATE",
        "SELECT id FROM orders OFFSET 2 LIMIT 1",
        "SELECT a FROM t WHERE a IS DISTINCT FROM b",
        "SELECT extract(year FROM d), substring(s FROM 1 FOR 2) FROM t",
        "SELECT 1 FROM t UNION SELECT 2 FROM u WHERE x = 1 ORDER BY 1",
        "SELECT a FROM t WHERE b IN (SELECT c FROM u GROUP BY c) ORDER BY a",
    ] {
        assert_eq!(
            loud(&found(Engine::Postgres, sql, None)),
            Found::new(),
            "{sql}"
        );
    }
}

// --- catalog findings -----------------------------------------------------------------------------

#[test]
fn unknown_relations_and_schemas() {
    let pg = sample(Engine::Postgres);
    let f = found(Engine::Postgres, "SELECT * FROM nope", Some(&pg));
    assert_eq!(f, vec![(UnknownRelation, Warning, "nope".to_owned())]);
    let f = found(Engine::Postgres, "SELECT * FROM sales.nope", Some(&pg));
    assert_eq!(f, vec![(UnknownRelation, Warning, "nope".to_owned())]);
    let f = found(Engine::Postgres, "SELECT * FROM nowhere.orders", Some(&pg));
    assert_eq!(f, vec![(UnknownSchema, Warning, "nowhere".to_owned())]);
    let f = found(Engine::Postgres, "SELECT * FROM \"Orders\"", Some(&pg));
    assert_eq!(f, vec![(UnknownRelation, Warning, "\"Orders\"".to_owned())]);
    // Quoted the catalog spelling, unquoted any case.
    assert_eq!(
        found(Engine::Postgres, "SELECT * FROM ORDERS", Some(&pg)),
        Found::new()
    );
    // System objects, other databases: hints.
    let f = found(Engine::Postgres, "SELECT * FROM pg_class", Some(&pg));
    assert_eq!(f, vec![(UnknownRelation, Hint, "pg_class".to_owned())]);
    let f = found(
        Engine::Postgres,
        "SELECT * FROM other.public.orders",
        Some(&pg),
    );
    assert_eq!(f, vec![(UnknownRelation, Hint, "orders".to_owned())]);
    let my = sample(Engine::MySql);
    let f = found(Engine::MySql, "SELECT * FROM archive.t", Some(&my));
    assert_eq!(f, vec![(UnknownRelation, Hint, "t".to_owned())]);
    // CTEs, temporary tables, tables created earlier in the script: nothing.
    for sql in [
        "WITH nope AS (SELECT 1 AS a) SELECT a FROM nope",
        "CREATE TABLE nope (a int); SELECT a FROM nope",
        "SELECT 1 AS a INTO nope FROM orders; SELECT a FROM nope",
        "ALTER TABLE orders RENAME TO nope; SELECT * FROM nope",
    ] {
        assert_eq!(
            found(Engine::Postgres, sql, Some(&pg)),
            Found::new(),
            "{sql}"
        );
    }
    let ms = sample(Engine::MsSql);
    assert_eq!(
        found(Engine::MsSql, "SELECT * FROM #nope", Some(&ms)),
        Found::new()
    );
    assert_eq!(
        found(Engine::MsSql, "SELECT * FROM shop.dbo.orders", Some(&ms)),
        Found::new()
    );
    // A truncated cache, another database or a changed search path: hints.
    let mut snap = sample_snapshot(Engine::Postgres);
    snap.truncated = true;
    let truncated = SchemaCache::new(snap);
    let f = found(Engine::Postgres, "SELECT * FROM nope", Some(&truncated));
    assert_eq!(f, vec![(UnknownRelation, Hint, "nope".to_owned())]);
    let f = found(Engine::MySql, "USE other; SELECT * FROM nope", Some(&my));
    assert_eq!(f, vec![(UnknownRelation, Hint, "nope".to_owned())]);
    let f = found(Engine::MySql, "USE shop; SELECT * FROM nope", Some(&my));
    assert_eq!(f, vec![(UnknownRelation, Warning, "nope".to_owned())]);
    let f = found(
        Engine::Postgres,
        "SET search_path TO x; SELECT * FROM nope",
        Some(&pg),
    );
    assert_eq!(f, vec![(UnknownRelation, Hint, "nope".to_owned())]);
    // Without a cache, nothing is known.
    assert_eq!(
        found(Engine::Postgres, "SELECT * FROM nope", None),
        Found::new()
    );
}

#[test]
fn unknown_and_ambiguous_columns() {
    let pg = sample(Engine::Postgres);
    let run = |sql: &str| found(Engine::Postgres, sql, Some(&pg));
    assert_eq!(
        run("SELECT nope FROM orders"),
        vec![(UnknownColumn, Warning, "nope".to_owned())]
    );
    assert_eq!(
        run("SELECT o.nope FROM orders o"),
        vec![(UnknownColumn, Warning, "nope".to_owned())]
    );
    assert_eq!(
        run("SELECT \"ID\" FROM orders"),
        vec![(UnknownColumn, Warning, "\"ID\"".to_owned())]
    );
    assert_eq!(
        run("SELECT id FROM orders ORDER BY nope"),
        vec![(UnknownColumn, Hint, "nope".to_owned())]
    );
    assert_eq!(
        run("SELECT nope FROM (SELECT id FROM orders) d"),
        vec![(UnknownColumn, Warning, "nope".to_owned())]
    );
    assert_eq!(
        run("WITH c AS (SELECT id FROM orders) SELECT c.nope FROM c"),
        vec![(UnknownColumn, Warning, "nope".to_owned())]
    );
    assert_eq!(
        run("INSERT INTO orders (nope) VALUES (1)"),
        vec![(UnknownColumn, Warning, "nope".to_owned())]
    );
    assert_eq!(
        run("UPDATE orders SET nope = 1 WHERE id = 1"),
        vec![(UnknownColumn, Warning, "nope".to_owned())]
    );
    assert_eq!(
        run("SELECT id FROM orders o JOIN customers c ON c.id = o.customer_id"),
        vec![(AmbiguousColumn, Warning, "id".to_owned())]
    );
    for sql in [
        "SELECT id AS x FROM orders ORDER BY x",
        "SELECT nope FROM (SELECT * FROM orders) d",
        "SELECT nope FROM orders, generate_series(1, 3) g",
        "SELECT id FROM orders JOIN invoices USING (id)",
        "SELECT o.id FROM orders o JOIN customers c ON c.id = o.customer_id",
        "SELECT id FROM orders WHERE EXISTS (SELECT 1 FROM customers c WHERE c.id = orders.customer_id)",
        "SELECT a, b FROM (VALUES (1, 2)) AS v(a, b)",
        "SELECT customer_id, total FROM orders WHERE total > 0 AND customer_id IN (SELECT id FROM customers)",
        "SELECT ctid FROM orders",
        "SELECT o FROM orders o",
    ] {
        assert_eq!(run(sql), Found::new(), "{sql}");
    }
    // Names that may be routine parameters or variables: hints.
    let my = sample(Engine::MySql);
    let f = found(
        Engine::MySql,
        "CREATE PROCEDURE p(IN x INT) BEGIN UPDATE orders SET total = x WHERE id = 1; END",
        Some(&my),
    );
    assert_eq!(f, vec![(UnknownColumn, Hint, "x".to_owned())]);
}

#[test]
fn function_calls() {
    let pg = sample(Engine::Postgres);
    let f = found(
        Engine::Postgres,
        "SELECT upper(name, 1) FROM customers",
        Some(&pg),
    );
    assert_eq!(
        f,
        vec![(WrongArgumentCount, Warning, "upper(name, 1)".to_owned())]
    );
    let f = found(Engine::Postgres, "SELECT count() FROM orders", Some(&pg));
    assert_eq!(f, vec![(WrongArgumentCount, Warning, "count()".to_owned())]);
    for sql in [
        "SELECT count(DISTINCT status) FROM orders",
        "SELECT count(*) FROM orders",
        "SELECT coalesce(notes, status, 'x') FROM orders",
        "SELECT lag(total, 1, 0) OVER (ORDER BY id) FROM orders",
    ] {
        assert_eq!(
            found(Engine::Postgres, sql, Some(&pg)),
            Found::new(),
            "{sql}"
        );
    }
    // Without a cache, functions are not checked.
    assert_eq!(
        found(Engine::Postgres, "SELECT count() FROM orders", None),
        Found::new()
    );
    // With an engine's own language data, unknown functions are hints.
    let dialect = Dialect::builtin(Engine::Postgres)
        .unwrap()
        .with_language(SqlLanguage {
            engine: Some(Engine::Postgres),
            ..SqlLanguage::standard()
        });
    let sql = "SELECT frobnicate(1), order_total(1), upper('a'), public.order_total(2), nope.x(1)";
    let f: Found = diagnose(&dialect, sql, Some(&pg), &CancellationToken::new())
        .unwrap()
        .into_iter()
        .map(|d| (d.code, d.severity, sql[d.range].to_owned()))
        .collect();
    assert_eq!(f, vec![(UnknownFunction, Hint, "frobnicate".to_owned())]);
}

// --- policy ---------------------------------------------------------------------------------------

#[test]
fn findings_are_sorted_and_never_quote_string_literals() {
    let pg = sample(Engine::Postgres);
    let sql = "SELECT 'secret-value', nope FROM nope2 WHERE x = 'other-secret';\nDELETE FROM orders;\nSELECT 'unterminated-secret";
    let diags = run(Engine::Postgres, sql, Some(&pg));
    assert!(diags.len() >= 3, "{diags:?}");
    for pair in diags.windows(2) {
        assert!(pair[0].range.start <= pair[1].range.start, "{diags:?}");
    }
    for d in &diags {
        assert!(!d.message.contains("secret"), "{}", d.message);
        assert!(d.severity <= d.code.max_severity());
    }
}

#[test]
fn a_cancelled_request_returns_cancelled() {
    let token = CancellationToken::new();
    token.cancel();
    let dialect = Dialect::builtin(Engine::Postgres).unwrap();
    let err = diagnose(&dialect, "SELECT 1; SELECT 2", None, &token).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Cancelled);
    // Empty text: nothing to do, nothing to cancel.
    assert_eq!(diagnose(&dialect, "", None, &token).unwrap(), Vec::new());
}

#[test]
fn a_one_mib_script_is_diagnosed_in_linear_time() {
    let base = corpus("postgres.sql");
    let mut text = String::new();
    while text.len() < 1024 * 1024 {
        text.push_str(&base);
        text.push('\n');
    }
    let pg = sample(Engine::Postgres);
    let dialect = Dialect::builtin(Engine::Postgres).unwrap();
    let started = Instant::now();
    let diags = diagnose(&dialect, &text, Some(&pg), &CancellationToken::new()).unwrap();
    let elapsed = started.elapsed();
    println!(
        "diagnose: {} KiB, {} findings, {elapsed:?} (debug build; the 100 ms budget is for release)",
        text.len() / 1024,
        diags.len()
    );
    assert!(elapsed.as_secs() < 30, "{elapsed:?}");
}
