//! Hover (`docs/m6-contracts.md` §4.4): relations, columns, aliases, CTEs, subqueries,
//! schemas, built-in and cached functions; nothing for keywords, literals and whitespace.

use oxbase_insight::{Dialect, Hover, SchemaCache, hover};
use oxbase_types::{
    CatalogColumn, CatalogForeignKey, CatalogRelation, CatalogRoutine, CatalogSchema,
    CatalogSnapshot, Engine, RelationKind, RoutineKind, ServerVersion,
};

fn column(name: &str, native_type: &str, nullable: bool, primary_key: bool) -> CatalogColumn {
    CatalogColumn {
        name: name.to_owned(),
        native_type: native_type.to_owned(),
        nullable,
        primary_key,
    }
}

fn cache() -> SchemaCache {
    let mut snap = CatalogSnapshot::new(Engine::Postgres, ServerVersion::new(16, 0, 0), "shop");
    snap.default_schema = Some("public".to_owned());
    snap.search_path = vec!["public".to_owned()];
    let mut public = CatalogSchema::new("public");
    public.relations = vec![
        CatalogRelation {
            name: "customers".to_owned(),
            kind: RelationKind::Table,
            columns: vec![
                column("id", "integer", false, true),
                column("name", "text", true, false),
            ],
            foreign_keys: Vec::new(),
            comment: Some("People who buy things".to_owned()),
        },
        CatalogRelation {
            name: "orders".to_owned(),
            kind: RelationKind::Table,
            columns: vec![
                column("id", "integer", false, true),
                column("customer_id", "integer", false, false),
                column("total", "numeric(10,2)", true, false),
            ],
            foreign_keys: vec![CatalogForeignKey {
                columns: vec!["customer_id".to_owned()],
                ref_schema: None,
                ref_table: "customers".to_owned(),
                ref_columns: vec!["id".to_owned()],
            }],
            comment: None,
        },
        CatalogRelation {
            name: "big_orders".to_owned(),
            kind: RelationKind::View,
            columns: vec![column("id", "integer", true, false)],
            foreign_keys: Vec::new(),
            comment: None,
        },
    ];
    public.routines = vec![CatalogRoutine {
        name: "order_total".to_owned(),
        kind: RoutineKind::Function,
        args: "order_id integer".to_owned(),
        returns: Some("numeric".to_owned()),
    }];
    let mut sales = CatalogSchema::new("sales");
    sales.relations = vec![CatalogRelation {
        name: "regions".to_owned(),
        kind: RelationKind::Table,
        columns: vec![column("id", "integer", false, true)],
        foreign_keys: Vec::new(),
        comment: None,
    }];
    snap.schemas = vec![public, sales];
    SchemaCache::new(snap)
}

/// The hover at the first occurrence of `marker` in `sql` (`marker` holds `|` where the cursor
/// is, e.g. `"o.to|tal"`).
fn at(sql: &str, marker: &str, cache: Option<&SchemaCache>) -> Option<Hover> {
    let cursor = marker.find('|').expect("marker has a cursor");
    let plain = marker.replace('|', "");
    let start = sql
        .find(&plain)
        .unwrap_or_else(|| panic!("`{plain}` not in `{sql}`"));
    let dialect = Dialect::builtin(Engine::Postgres).unwrap();
    hover(&dialect, sql, start + cursor, cache)
}

fn md(sql: &str, marker: &str, cache: Option<&SchemaCache>) -> String {
    at(sql, marker, cache)
        .unwrap_or_else(|| panic!("no hover at `{marker}` in `{sql}`"))
        .markdown
}

#[test]
fn relations_show_kind_columns_keys_and_comment() {
    let c = cache();
    let sql = "SELECT o.total FROM orders o";
    let h = at(sql, "FROM ord|ers", Some(&c)).unwrap();
    assert_eq!(&sql[h.range.clone()], "orders");
    assert!(
        h.markdown.starts_with("**table** `public.orders`"),
        "{}",
        h.markdown
    );
    assert!(
        h.markdown
            .contains("| `customer_id` | integer | not null | FK → customers(id) |"),
        "{}",
        h.markdown
    );
    assert!(
        h.markdown.contains("| `id` | integer | not null | PK |"),
        "{}",
        h.markdown
    );
    let m = md("SELECT * FROM customers", "custo|mers", Some(&c));
    assert!(m.contains("People who buy things"), "{m}");
    let m = md("SELECT * FROM big_orders", "big|_orders", Some(&c));
    assert!(m.starts_with("**view** `public.big_orders`"), "{m}");
    let m = md("SELECT * FROM sales.regions", "sal|es", Some(&c));
    assert!(m.starts_with("**schema** `sales`"), "{m}");
    let m = md("SELECT * FROM sales.regions", "reg|ions", Some(&c));
    assert!(m.starts_with("**table** `sales.regions`"), "{m}");
}

#[test]
fn aliases_and_qualifiers_stand_for_their_relation() {
    let c = cache();
    let sql = "SELECT o.total FROM orders o";
    let m = md(sql, "|o.total", Some(&c));
    assert!(
        m.starts_with("**alias** `o` of **table** `public.orders`"),
        "{m}"
    );
    let m = md(sql, "orders |o", Some(&c));
    assert!(m.starts_with("**alias** `o` of **table**"), "{m}");
}

#[test]
fn columns_show_type_nullability_keys_and_relation() {
    let c = cache();
    let m = md("SELECT o.total FROM orders o", "o.to|tal", Some(&c));
    assert!(
        m.starts_with("**column** `orders.total` · `numeric(10,2)` · null"),
        "{m}"
    );
    assert!(m.contains("In table `public.orders`"), "{m}");
    let m = md("SELECT customer_id FROM orders", "customer|_id", Some(&c));
    assert!(m.contains("not null · FK → customers(id)"), "{m}");
    let m = md(
        "SELECT name FROM orders o JOIN customers c ON c.id = o.customer_id",
        "na|me",
        Some(&c),
    );
    assert!(m.starts_with("**column** `customers.name`"), "{m}");
    // An ambiguous column has no single owner.
    assert_eq!(
        at(
            "SELECT id FROM orders o JOIN customers c ON c.id = o.customer_id",
            "SELECT i|d",
            Some(&c)
        ),
        None
    );
    // A cursor right after the name still hovers it.
    let m = md("SELECT total FROM orders", "total|", Some(&c));
    assert!(m.starts_with("**column** `orders.total`"), "{m}");
}

#[test]
fn ctes_and_subqueries() {
    let c = cache();
    let sql = "WITH recent AS (SELECT id, total FROM orders) SELECT r.total FROM recent r";
    let m = md(sql, "WITH rec|ent", Some(&c));
    assert_eq!(m, "**CTE** `recent`\n\nColumns: `id`, `total`");
    let m = md(sql, "FROM rec|ent", None);
    assert_eq!(m, "**CTE** `recent`\n\nColumns: `id`, `total`");
    let m = md(sql, "r.to|tal", Some(&c));
    assert!(m.starts_with("**column** `total` of CTE `r`"), "{m}");
    let sql = "SELECT d.n FROM (SELECT id AS n FROM orders) d";
    let m = md(sql, ") |d", None);
    assert_eq!(m, "**alias** `d` of **subquery**\n\nColumns: `n`");
    let m = md(sql, "d.|n", None);
    assert!(m.starts_with("**column** `n` of subquery `d`"), "{m}");
}

#[test]
fn functions_show_signatures() {
    let c = cache();
    let m = md("SELECT upper(name) FROM customers", "up|per", None);
    assert!(m.starts_with("```sql\nUPPER("), "{m}");
    let m = md(
        "SELECT order_total(id) FROM orders",
        "order_|total",
        Some(&c),
    );
    assert_eq!(
        m,
        "**function** `public.order_total(order_id integer)` → `numeric`"
    );
    assert_eq!(at("SELECT nothing_known(1)", "nothing|", Some(&c)), None);
}

#[test]
fn nothing_for_keywords_literals_whitespace_and_unknown_names() {
    let c = cache();
    let sql = "SELECT 'text', 42  FROM orders WHERE nope = 1";
    assert_eq!(at(sql, "SEL|ECT", Some(&c)), None);
    assert_eq!(at(sql, "'te|xt'", Some(&c)), None);
    assert_eq!(at(sql, "4|2", Some(&c)), None);
    assert_eq!(at(sql, "42 | FROM", Some(&c)), None);
    assert_eq!(at(sql, "no|pe", Some(&c)), None);
    // Without a cache a table is unknown.
    assert_eq!(at("SELECT * FROM orders", "ord|ers", None), None);
    // Out-of-range offsets and empty text.
    let dialect = Dialect::builtin(Engine::Postgres).unwrap();
    assert_eq!(hover(&dialect, "", 5, Some(&c)), None);
    assert_eq!(hover(&dialect, "SELECT 1", 500, Some(&c)), None);
}

#[test]
fn every_offset_hovers_without_panicking() {
    let c = cache();
    let texts = [
        "WITH r AS (SELECT id FROM orders) SELECT r.id, o.total, upper(x.name) FROM r JOIN orders o ON o.id = r.id, (SELECT name FROM customers) x WHERE o.total > 1;",
        "INSERT INTO orders (id, total) VALUES (1, 2) ON CONFLICT (id) DO UPDATE SET total = excluded.total RETURNING id;",
        "UPDATE orders SET total = 0 FROM customers c WHERE c.id = orders.customer_id; DELETE FROM orders WHERE id = 'é😀';",
        "SELECT \"unterminated FROM (((",
    ];
    for engine in [
        Engine::Postgres,
        Engine::MySql,
        Engine::MariaDb,
        Engine::Sqlite,
        Engine::MsSql,
    ] {
        let dialect = Dialect::builtin(engine).unwrap();
        for text in texts {
            for offset in 0..=text.len() + 1 {
                let h = hover(&dialect, text, offset, Some(&c));
                if let Some(h) = h {
                    assert!(h.range.start <= h.range.end && h.range.end <= text.len());
                    assert!(!h.markdown.is_empty());
                }
                let _ = hover(&dialect, text, offset, None);
            }
        }
    }
}
