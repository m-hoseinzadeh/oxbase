//! Completion behaviour (`docs/m6-contracts.md` §4.3, §9): sources, ranking, replace ranges,
//! quoting per dialect, signatures, joins from foreign keys, recency and cancellation.
//!
//! The cursor contexts are built by hand (what `context_at` reads at each cursor), so these
//! tests pin `complete` alone; the context stream's own tests cover `context_at`.

use oxbase_dialect::{FunctionInfo, FunctionKind, SqlLanguage};
use oxbase_insight::{
    CancellationToken, Clause, Completion, CompletionKind, CompletionList, Cte, CursorContext,
    Dialect, Expected, Ident, Outline, QueryBlock, RecentNames, RelationSource, SchemaCache,
    ScopeRelation, StatementClass, complete, complete_with_recent,
};
use oxbase_types::{
    CaseRules, CatalogColumn, CatalogForeignKey, CatalogRelation, CatalogRoutine, CatalogSchema,
    CatalogSnapshot, Engine, ErrorCategory, IdentCase, RelationKind, RoutineKind, ServerVersion,
    Snippet,
};

// --- fixtures -------------------------------------------------------------------------------

fn col(name: &str, ty: &str, nullable: bool, pk: bool) -> CatalogColumn {
    CatalogColumn {
        name: name.into(),
        native_type: ty.into(),
        nullable,
        primary_key: pk,
    }
}

fn fk(column: &str, table: &str) -> CatalogForeignKey {
    CatalogForeignKey {
        columns: vec![column.into()],
        ref_schema: None,
        ref_table: table.into(),
        ref_columns: vec!["id".into()],
    }
}

fn rel(
    name: &str,
    kind: RelationKind,
    columns: Vec<CatalogColumn>,
    fks: Vec<CatalogForeignKey>,
) -> CatalogRelation {
    CatalogRelation {
        name: name.into(),
        kind,
        columns,
        foreign_keys: fks,
        comment: None,
    }
}

/// A small shop schema: `public` (on the search path) and `sales`.
fn snapshot(engine: Engine) -> CatalogSnapshot {
    let mut snap = CatalogSnapshot::new(engine, ServerVersion::new(16, 0, 0), "shop");
    snap.databases = vec!["shop".into(), "archive".into()];
    snap.default_schema = Some("public".into());
    snap.search_path = vec!["public".into()];
    let mut public = CatalogSchema::new("public");
    let mut customers = rel(
        "customers",
        RelationKind::Table,
        vec![
            col("id", "bigint", false, true),
            col("name", "text", false, false),
            col("Email Address", "text", true, false),
        ],
        vec![],
    );
    customers.comment = Some("People who buy things.".into());
    public.relations = vec![
        customers,
        rel(
            "orders",
            RelationKind::Table,
            vec![
                col("id", "bigint", false, true),
                col("customer_id", "bigint", false, false),
                col("total", "numeric(12,2)", true, false),
                col("created_at", "timestamp", false, false),
                col("order", "integer", true, false),
            ],
            vec![fk("customer_id", "customers")],
        ),
        rel(
            "Order Lines",
            RelationKind::Table,
            vec![
                col("order_id", "bigint", false, false),
                col("qty", "integer", false, false),
            ],
            vec![CatalogForeignKey {
                columns: vec!["order_id".into()],
                ref_schema: None,
                ref_table: "orders".into(),
                ref_columns: vec!["id".into()],
            }],
        ),
        rel(
            "order_totals",
            RelationKind::View,
            vec![col("total", "numeric", true, false)],
            vec![],
        ),
        rel(
            "AuditLog",
            RelationKind::Table,
            vec![col("id", "bigint", false, true)],
            vec![],
        ),
    ];
    public.routines = vec![
        CatalogRoutine {
            name: "get_total".into(),
            kind: RoutineKind::Function,
            args: "p_order bigint".into(),
            returns: Some("numeric".into()),
        },
        CatalogRoutine {
            name: "now_utc".into(),
            kind: RoutineKind::Function,
            args: String::new(),
            returns: Some("timestamp".into()),
        },
        CatalogRoutine {
            name: "refresh_stats".into(),
            kind: RoutineKind::Procedure,
            args: "p_full boolean".into(),
            returns: None,
        },
    ];
    public.types = vec!["mood".into()];
    public.sequences = vec!["order_seq".into()];
    let mut sales = CatalogSchema::new("sales");
    sales.relations = vec![
        rel(
            "orders",
            RelationKind::Table,
            vec![col("id", "bigint", false, true)],
            vec![],
        ),
        rel(
            "invoices",
            RelationKind::Table,
            vec![col("id", "bigint", false, true)],
            vec![],
        ),
    ];
    sales.routines = vec![CatalogRoutine {
        name: "get_margin".into(),
        kind: RoutineKind::Function,
        args: "p bigint".into(),
        returns: Some("numeric".into()),
    }];
    snap.schemas = vec![public, sales];
    snap
}

fn cache(engine: Engine) -> SchemaCache {
    SchemaCache::new(snapshot(engine))
}

fn ident(text: &str) -> Ident {
    let quoted = text.starts_with('"');
    Ident {
        text: text.trim_matches('"').to_owned(),
        quoted,
    }
}

fn named(path: &str, alias: Option<&str>) -> ScopeRelation {
    ScopeRelation {
        source: RelationSource::Named(path.split('.').map(ident).collect()),
        alias: alias.map(ident),
        column_aliases: vec![],
        range: 0..0,
    }
}

/// The context at `<|>` in `text`: the word being typed (with an opening quote), its prefix
/// and qualifier, as `context_at` reports them.
fn ctx(text: &str, expected: &[Expected], clause: Clause) -> CursorContext {
    let offset = text.find("<|>").expect("cursor marker");
    let text = text.replace("<|>", "");
    let bytes = text.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = offset;
    while start > 0 && is_word(bytes[start - 1]) {
        start -= 1;
    }
    // A quoted identifier being typed: back to its opening quote.
    let mut quote = None;
    if let Some(open) = text[..offset].rfind(['"', '`', '[']) {
        let inside = &text[open + 1..offset];
        if !inside.contains(['"', '`', ']', '.', ',', '(']) {
            quote = Some(bytes[open]);
            start = open;
        }
    }
    let mut end = offset;
    while end < bytes.len() && is_word(bytes[end]) {
        end += 1;
    }
    let prefix_start = if quote.is_some() { start + 1 } else { start };
    let prefix = text[prefix_start..offset].to_owned();
    // `a.b.<|>`: the parts before the dot.
    let mut qualifier = Vec::new();
    let mut at = start;
    while at > 0 && bytes[at - 1] == b'.' {
        let part_end = at - 1;
        let mut part_start = part_end;
        while part_start > 0 && is_word(bytes[part_start - 1]) {
            part_start -= 1;
        }
        qualifier.insert(0, ident(&text[part_start..part_end]));
        at = part_start;
    }
    CursorContext {
        offset,
        statement: 0..text.len(),
        class: StatementClass::Select,
        clause,
        word: start..end,
        prefix,
        quote,
        qualifier,
        expected: expected.to_vec(),
        keywords: vec![],
        scope: vec![],
        ctes: vec![],
        outline: Outline::empty(0..text.len()),
    }
}

fn pg() -> Dialect {
    Dialect::builtin(Engine::Postgres).unwrap()
}

fn run(c: &CursorContext, d: &Dialect, cache: Option<&SchemaCache>) -> CompletionList {
    complete(c, d, cache, &[], 200, &CancellationToken::new()).unwrap()
}

fn labels(list: &CompletionList) -> Vec<&str> {
    list.items.iter().map(|i| i.label.as_str()).collect()
}

fn find<'a>(list: &'a CompletionList, label: &str) -> &'a Completion {
    list.items
        .iter()
        .find(|i| i.label == label)
        .unwrap_or_else(|| panic!("no item {label} in {:?}", labels(list)))
}

fn position(list: &CompletionList, label: &str) -> usize {
    list.items
        .iter()
        .position(|i| i.label == label)
        .unwrap_or_else(|| panic!("no item {label} in {:?}", labels(list)))
}

const SELECT_LIST: &[Expected] = &[Expected::Column, Expected::Function, Expected::Keyword];
const FROM: &[Expected] = &[Expected::Relation, Expected::Schema];

fn assert_well_formed(list: &CompletionList, c: &CursorContext) {
    for pair in list.items.windows(2) {
        assert!(
            pair[0].sort_key < pair[1].sort_key,
            "sort keys must increase"
        );
    }
    for item in &list.items {
        assert_eq!(item.replace, c.word, "{} replaces the word", item.label);
    }
}

// --- statement start, keywords, snippets ----------------------------------------------------

#[test]
fn statement_start_offers_keywords_then_snippets() {
    let mut c = ctx(
        "<|>",
        &[Expected::Keyword, Expected::Snippet],
        Clause::Start,
    );
    c.keywords = vec!["SELECT", "INSERT", "UPDATE", "WITH"];
    let snippets = oxbase_insight::snippets::builtin_snippets();
    let list = complete(&c, &pg(), None, &snippets, 500, &CancellationToken::new()).unwrap();
    assert_well_formed(&list, &c);
    assert_eq!(&labels(&list)[..4], ["SELECT", "INSERT", "UPDATE", "WITH"]);
    // Any other keyword at statement level, after the likely ones.
    let commit = position(&list, "COMMIT");
    assert!(commit > 3);
    // Snippets last; only the ones for the engine.
    let first_snippet = list
        .items
        .iter()
        .position(|i| i.kind == CompletionKind::Snippet)
        .unwrap();
    assert!(first_snippet > commit);
    assert!(
        list.items[first_snippet..]
            .iter()
            .all(|i| i.kind == CompletionKind::Snippet)
    );
    let sell = list
        .items
        .iter()
        .filter(|i| i.label == "sell")
        .collect::<Vec<_>>();
    assert_eq!(sell.len(), 1);
    assert!(sell[0].insert_text.contains("LIMIT"));
    assert!(sell[0].is_snippet);
    assert_eq!(sell[0].detail.as_deref(), Some("Select first rows"));

    let mut c = ctx(
        "sel<|>",
        &[Expected::Keyword, Expected::Snippet],
        Clause::Start,
    );
    c.keywords = vec!["SELECT", "INSERT"];
    let list = complete(&c, &pg(), None, &snippets, 50, &CancellationToken::new()).unwrap();
    assert_well_formed(&list, &c);
    // Typed in lower case: the keyword comes in lower case; the snippet named `sel` follows.
    assert_eq!(list.items[0].label, "select");
    assert_eq!(list.items[0].insert_text, "select");
    assert_eq!(list.items[1].kind, CompletionKind::Snippet);
    assert_eq!(list.items[1].label, "sel");

    let mut c = ctx("SEL<|>", &[Expected::Keyword], Clause::Start);
    c.keywords = vec!["SELECT"];
    assert_eq!(run(&c, &pg(), None).items[0].insert_text, "SELECT");
}

#[test]
fn listed_keywords_come_in_their_order_and_alone_inside_a_clause() {
    let mut c = ctx(
        "SELECT * FROM orders o <|>",
        &[Expected::Keyword],
        Clause::From,
    );
    c.keywords = vec!["WHERE", "JOIN", "LEFT", "GROUP", "ORDER", "LIMIT"];
    let list = run(&c, &pg(), Some(&cache(Engine::Postgres)));
    assert_eq!(
        labels(&list),
        ["WHERE", "JOIN", "LEFT", "GROUP", "ORDER", "LIMIT"]
    );
    c.prefix = "wh".into();
    c.word = c.offset..c.offset;
    let list = run(&c, &pg(), None);
    assert_eq!(labels(&list), ["where"]);
}

#[test]
fn nothing_is_offered_where_nothing_is_expected() {
    let c = ctx("SELECT '<|>'", &[], Clause::SelectList);
    let list = run(&c, &pg(), Some(&cache(Engine::Postgres)));
    assert_eq!(list, CompletionList::default());
    let list = complete(
        &ctx("<|>", &[Expected::Keyword], Clause::Start),
        &pg(),
        None,
        &[],
        0,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(list.items.len(), 0);
}

// --- relations --------------------------------------------------------------------------------

#[test]
fn relations_rank_exact_then_search_path_then_other_schemas() {
    let cache = cache(Engine::Postgres);
    let c = ctx("SELECT * FROM ord<|>", FROM, Clause::From);
    let list = run(&c, &pg(), Some(&cache));
    assert_well_formed(&list, &c);
    assert_eq!(
        labels(&list)[..4],
        ["orders", "Order Lines", "order_totals", "orders"]
    );
    let first = &list.items[0];
    assert_eq!(first.kind, CompletionKind::Table);
    assert_eq!(first.insert_text, "orders");
    assert_eq!(first.detail.as_deref(), Some("table · public"));
    assert_eq!(first.filter_text, None);
    // Mixed case and a space: quoted on PostgreSQL.
    let lines = &list.items[1];
    assert_eq!(lines.insert_text, "\"Order Lines\"");
    assert_eq!(lines.filter_text.as_deref(), Some("Order Lines"));
    assert_eq!(list.items[2].kind, CompletionKind::View);
    assert_eq!(list.items[2].detail.as_deref(), Some("view · public"));
    // Outside the search path: inserted qualified.
    assert_eq!(list.items[3].insert_text, "sales.orders");
    assert_eq!(list.items[3].detail.as_deref(), Some("table · sales"));

    // An exact match ranks first.
    let c = ctx("SELECT * FROM order_totals<|>", FROM, Clause::From);
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(list.items[0].label, "order_totals");
    assert!(!list.incomplete);
}

#[test]
fn an_empty_prefix_lists_relations_then_schemas_with_comments() {
    let cache = cache(Engine::Postgres);
    let c = ctx("SELECT * FROM <|>", FROM, Clause::From);
    let list = run(&c, &pg(), Some(&cache));
    assert_well_formed(&list, &c);
    let customers = find(&list, "customers");
    assert_eq!(
        customers.documentation.as_deref(),
        Some("People who buy things.")
    );
    let schema = position(&list, "sales");
    assert_eq!(list.items[schema].kind, CompletionKind::Schema);
    assert_eq!(list.items[schema].detail.as_deref(), Some("schema"));
    assert!(position(&list, "invoices") < schema);
    assert!(position(&list, "customers") < position(&list, "invoices"));
}

#[test]
fn fuzzy_matches_fill_the_list_when_prefixes_find_too_few() {
    let cache = cache(Engine::Postgres);
    let c = ctx("SELECT * FROM ordtot<|>", FROM, Clause::From);
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(labels(&list)[0], "order_totals");
    let c = ctx("SELECT * FROM xyz<|>", FROM, Clause::From);
    assert_eq!(run(&c, &pg(), Some(&cache)).items.len(), 0);
}

#[test]
fn schema_qualified_relations() {
    let cache = cache(Engine::Postgres);
    let c = ctx(
        "SELECT * FROM sales.<|>",
        &[Expected::Relation],
        Clause::From,
    );
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(labels(&list), ["orders", "invoices"]);
    assert_eq!(list.items[0].insert_text, "orders");
    let c = ctx(
        "SELECT * FROM shop.sales.inv<|>",
        &[Expected::Relation],
        Clause::From,
    );
    assert_eq!(labels(&run(&c, &pg(), Some(&cache))), ["invoices"]);
    // Another database: nothing known.
    let c = ctx(
        "SELECT * FROM archive.sales.<|>",
        &[Expected::Relation],
        Clause::From,
    );
    assert_eq!(run(&c, &pg(), Some(&cache)).items.len(), 0);
}

#[test]
fn quoted_prefixes_match_exactly_and_keep_the_quote() {
    let cache = cache(Engine::Postgres);
    let c = ctx("SELECT * FROM \"Ord<|>", FROM, Clause::From);
    assert_eq!(c.quote, Some(b'"'));
    let list = run(&c, &pg(), Some(&cache));
    assert_well_formed(&list, &c);
    assert_eq!(labels(&list), ["Order Lines"]);
    assert_eq!(list.items[0].insert_text, "\"Order Lines\"");
    assert_eq!(list.items[0].filter_text.as_deref(), Some("\"Order Lines"));

    let c = ctx("SELECT * FROM \"ord<|>", FROM, Clause::From);
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(labels(&list), ["orders", "order_totals", "orders"]);
    assert_eq!(list.items[0].insert_text, "\"orders\"");
    assert_eq!(list.items[2].insert_text, "sales.\"orders\"");

    // T-SQL brackets.
    let mssql = Dialect::builtin(Engine::MsSql).unwrap();
    let cache = self::cache(Engine::MsSql);
    let c = ctx("SELECT * FROM [Order<|>", FROM, Clause::From);
    let list = run(&c, &mssql, Some(&cache));
    assert_eq!(labels(&list), ["Order Lines"]);
    assert_eq!(list.items[0].insert_text, "[Order Lines]");
}

#[test]
fn names_are_quoted_per_dialect_and_case_rules() {
    let quoted = |engine: Engine, name: &str| -> String {
        let d = Dialect::builtin(engine).unwrap();
        let cache = cache(engine);
        let c = ctx("SELECT * FROM <|>", FROM, Clause::From);
        find(&run(&c, &d, Some(&cache)), name).insert_text.clone()
    };
    assert_eq!(quoted(Engine::Postgres, "AuditLog"), "\"AuditLog\"");
    assert_eq!(quoted(Engine::MsSql, "AuditLog"), "AuditLog");
    assert_eq!(quoted(Engine::MySql, "AuditLog"), "AuditLog");
    assert_eq!(quoted(Engine::Sqlite, "AuditLog"), "AuditLog");
    assert_eq!(quoted(Engine::Postgres, "Order Lines"), "\"Order Lines\"");
    assert_eq!(quoted(Engine::MySql, "Order Lines"), "`Order Lines`");
    assert_eq!(quoted(Engine::MsSql, "Order Lines"), "[Order Lines]");
    assert_eq!(quoted(Engine::Sqlite, "Order Lines"), "\"Order Lines\"");
}

#[test]
fn cte_names_count_as_relations() {
    let mut c = ctx(
        "WITH recent AS (SELECT 1) SELECT * FROM re<|>",
        FROM,
        Clause::From,
    );
    c.ctes = vec![Cte {
        name: ident("recent"),
        columns: vec![],
        query: None,
        range: 5..25,
    }];
    let list = run(&c, &pg(), Some(&cache(Engine::Postgres)));
    let item = find(&list, "recent");
    assert_eq!(item.kind, CompletionKind::Alias);
    assert_eq!(item.detail.as_deref(), Some("CTE"));
    assert_eq!(item.insert_text, "recent");
    // Without a cache the CTE is still there.
    assert_eq!(labels(&run(&c, &pg(), None)), ["recent"]);
}

#[test]
fn joins_follow_foreign_keys_both_ways() {
    let cache = cache(Engine::Postgres);
    let mut c = ctx("SELECT * FROM orders o JOIN <|>", FROM, Clause::Join);
    c.scope = vec![named("orders", Some("o"))];
    let list = run(&c, &pg(), Some(&cache));
    assert_well_formed(&list, &c);
    assert_eq!(list.items[0].kind, CompletionKind::Join);
    let joins: Vec<&str> = list
        .items
        .iter()
        .filter(|i| i.kind == CompletionKind::Join)
        .map(|i| i.label.as_str())
        .collect();
    assert_eq!(
        joins,
        [
            "customers c ON c.id = o.customer_id",
            "\"Order Lines\" ol ON ol.order_id = o.id",
        ]
    );
    let item = find(&list, "customers c ON c.id = o.customer_id");
    assert!(item.is_snippet);
    assert_eq!(item.filter_text.as_deref(), Some("customers"));
    // Joins come before the plain relations.
    assert!(position(&list, "customers") > 1);

    // The alias avoids names in scope and reserved words.
    let mut c = ctx("SELECT * FROM orders c JOIN cu<|>", FROM, Clause::Join);
    c.scope = vec![named("orders", Some("c"))];
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(list.items[0].label, "customers c2 ON c2.id = c.customer_id");

    // Not after a qualifier, not outside a JOIN.
    let mut c = ctx("SELECT * FROM orders o, <|>", FROM, Clause::From);
    c.scope = vec![named("orders", Some("o"))];
    let list = run(&c, &pg(), Some(&cache));
    assert!(list.items.iter().all(|i| i.kind != CompletionKind::Join));
}

// --- columns ----------------------------------------------------------------------------------

#[test]
fn select_list_offers_scope_columns_then_aliases_functions_keywords() {
    let cache = cache(Engine::Postgres);
    let mut c = ctx("SELECT <|> FROM orders o", SELECT_LIST, Clause::SelectList);
    c.scope = vec![named("orders", Some("o"))];
    c.keywords = vec!["DISTINCT", "CASE"];
    let list = run(&c, &pg(), Some(&cache));
    assert_well_formed(&list, &c);
    assert_eq!(
        labels(&list)[..6],
        ["id", "order", "total", "created_at", "customer_id", "o"]
    );
    assert!(
        list.items[..5]
            .iter()
            .all(|i| i.kind == CompletionKind::Column)
    );
    assert_eq!(list.items[5].kind, CompletionKind::Alias);
    assert_eq!(list.items[5].detail.as_deref(), Some("alias · orders"));
    // A reserved word is quoted.
    assert_eq!(find(&list, "order").insert_text, "\"order\"");
    assert_eq!(
        find(&list, "customer_id").detail.as_deref(),
        Some("bigint · not null · FK → customers")
    );
    assert_eq!(
        find(&list, "id").detail.as_deref(),
        Some("bigint · not null · PK")
    );
    assert_eq!(
        find(&list, "total").documentation.as_deref(),
        Some("Column of `public.orders`")
    );
    // Functions before keywords; the listed keywords only.
    let count = position(&list, "COUNT");
    assert!(count > 5);
    assert!(position(&list, "DISTINCT") > count);
    assert!(list.items.iter().all(|i| i.label != "SELECT"));
}

#[test]
fn ambiguous_columns_are_inserted_qualified() {
    let cache = cache(Engine::Postgres);
    let mut c = ctx(
        "SELECT i<|> FROM orders o JOIN customers c ON true",
        SELECT_LIST,
        Clause::SelectList,
    );
    c.scope = vec![named("orders", Some("o")), named("customers", Some("c"))];
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(labels(&list)[..2], ["c.id", "o.id"]);
    assert_eq!(list.items[0].insert_text, "c.id");
    assert_eq!(list.items[0].filter_text.as_deref(), Some("id"));
}

#[test]
fn a_qualifier_limits_columns_to_its_relation() {
    let cache = cache(Engine::Postgres);
    let mut c = ctx(
        "SELECT c.<|> FROM orders o JOIN customers c ON true",
        SELECT_LIST,
        Clause::SelectList,
    );
    c.scope = vec![named("orders", Some("o")), named("customers", Some("c"))];
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(labels(&list), ["id", "name", "Email Address"]);
    assert_eq!(list.items[2].insert_text, "\"Email Address\"");
    let mysql = Dialect::builtin(Engine::MySql).unwrap();
    let list = run(&c, &mysql, Some(&self::cache(Engine::MySql)));
    assert_eq!(find(&list, "Email Address").insert_text, "`Email Address`");

    // A relation name not in scope (no FROM yet).
    let c = ctx(
        "SELECT customers.na<|>",
        &[Expected::Column, Expected::Relation],
        Clause::SelectList,
    );
    assert_eq!(labels(&run(&c, &pg(), Some(&cache))), ["name"]);
    // A schema as qualifier.
    let c = ctx(
        "SELECT sales.<|>",
        &[Expected::Relation, Expected::Column],
        Clause::SelectList,
    );
    assert_eq!(
        labels(&run(&c, &pg(), Some(&cache))),
        ["orders", "invoices"]
    );
}

#[test]
fn ctes_and_derived_tables_give_their_columns() {
    let cache = cache(Engine::Postgres);
    let mut c = ctx(
        "WITH x AS (SELECT id, created_at FROM orders) SELECT x.<|> FROM x",
        SELECT_LIST,
        Clause::SelectList,
    );
    let body = QueryBlock {
        range: 11..43,
        parent: Some(0),
        ctes: vec![],
        relations: vec![named("orders", None)],
        output: vec![ident("id"), ident("created_at")],
        star: false,
        clauses: vec![],
    };
    let top = QueryBlock {
        range: 0..60,
        parent: None,
        ctes: vec![Cte {
            name: ident("x"),
            columns: vec![ident("id"), ident("created_at")],
            query: Some(1),
            range: 5..44,
        }],
        relations: vec![ScopeRelation {
            source: RelationSource::Cte { query: 0, index: 0 },
            alias: None,
            column_aliases: vec![],
            range: 56..57,
        }],
        output: vec![],
        star: false,
        clauses: vec![],
    };
    c.outline = Outline {
        range: 0..60,
        class: StatementClass::Select,
        queries: vec![top.clone(), body],
        refs: vec![],
    };
    c.scope.clone_from(&top.relations);
    c.ctes.clone_from(&top.ctes);
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(labels(&list), ["id", "created_at"]);
    assert_eq!(
        list.items[0].detail.as_deref(),
        Some("bigint · not null · PK")
    );

    // A derived table with an alias column list.
    let mut c = ctx(
        "SELECT d.<|> FROM (SELECT 1, 2) AS d(a, b)",
        SELECT_LIST,
        Clause::SelectList,
    );
    c.scope = vec![ScopeRelation {
        source: RelationSource::Derived {
            query: 1,
            columns: vec![],
        },
        alias: Some(ident("d")),
        column_aliases: vec![ident("a"), ident("b")],
        range: 0..0,
    }];
    assert_eq!(labels(&run(&c, &pg(), Some(&cache))), ["a", "b"]);
}

#[test]
fn without_a_scope_columns_come_from_the_whole_cache_by_prefix() {
    let cache = cache(Engine::Postgres);
    let c = ctx("SELECT cre<|>", SELECT_LIST, Clause::SelectList);
    let list = run(&c, &pg(), Some(&cache));
    let item = find(&list, "created_at");
    assert_eq!(item.kind, CompletionKind::Column);
    // Nothing typed and nothing in scope: no columns.
    let c = ctx("SELECT <|>", SELECT_LIST, Clause::SelectList);
    let list = run(&c, &pg(), Some(&cache));
    assert!(list.items.iter().all(|i| i.kind != CompletionKind::Column));
}

#[test]
fn insert_and_update_targets_give_their_columns() {
    let cache = cache(Engine::Postgres);
    let mut c = ctx(
        "INSERT INTO customers (<|>",
        &[Expected::Column],
        Clause::InsertColumns,
    );
    c.scope = vec![named("customers", None)];
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(labels(&list), ["id", "name", "Email Address"]);
    let mut c = ctx(
        "UPDATE orders SET to<|>",
        &[Expected::Column],
        Clause::UpdateSet,
    );
    c.scope = vec![named("orders", None)];
    let list = run(&c, &pg(), Some(&cache));
    // The prefix match first, fuzzy matches after it; no aliases.
    assert_eq!(labels(&list)[0], "total");
    assert!(list.items.iter().all(|i| i.kind == CompletionKind::Column));
}

// --- functions, procedures, types -------------------------------------------------------------

#[test]
fn functions_carry_signatures_and_call_snippets() {
    let cache = cache(Engine::Postgres);
    let c = ctx(
        "SELECT get_<|> FROM orders",
        SELECT_LIST,
        Clause::SelectList,
    );
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(labels(&list), ["get_total", "get_margin"]);
    let f = &list.items[0];
    assert_eq!(f.kind, CompletionKind::Function);
    assert_eq!(f.insert_text, "get_total($0)");
    assert!(f.is_snippet);
    assert_eq!(
        f.detail.as_deref(),
        Some("get_total(p_order bigint) → numeric")
    );
    assert_eq!(list.items[1].insert_text, "sales.get_margin($0)");

    let c = ctx("SELECT now_<|>", SELECT_LIST, Clause::SelectList);
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(list.items[0].insert_text, "now_utc()");
    assert!(!list.items[0].is_snippet);

    // Built-in functions: the typed case decides the inserted case.
    let c = ctx("SELECT cou<|>", SELECT_LIST, Clause::SelectList);
    let list = run(&c, &pg(), None);
    assert_eq!(list.items[0].label, "count");
    assert_eq!(list.items[0].insert_text, "count($0)");
    assert_eq!(list.items[0].filter_text.as_deref(), Some("COUNT"));
    assert!(
        list.items[0]
            .detail
            .as_deref()
            .unwrap()
            .starts_with("count(")
    );
    assert!(list.items[0].documentation.is_some());
    let c = ctx("SELECT COU<|>", SELECT_LIST, Clause::SelectList);
    assert_eq!(run(&c, &pg(), None).items[0].insert_text, "COUNT($0)");
    let c = ctx("SELECT ROW_<|>", SELECT_LIST, Clause::SelectList);
    let list = run(&c, &pg(), None);
    assert_eq!(list.items[0].label, "ROW_NUMBER");
    assert_eq!(list.items[0].insert_text, "ROW_NUMBER()");
}

static VERSIONED: &[FunctionInfo] = &[
    FunctionInfo {
        name: "NEW_FN",
        args: "x",
        returns: "integer",
        kind: FunctionKind::Scalar,
        min_args: 1,
        max_args: Some(1),
        summary: "Only on version 99.",
        since: Some((99, 0)),
    },
    FunctionInfo {
        name: "OLD_FN",
        args: "x",
        returns: "integer",
        kind: FunctionKind::Scalar,
        min_args: 1,
        max_args: Some(1),
        summary: "Everywhere.",
        since: None,
    },
    FunctionInfo {
        name: "OLD_FN",
        args: "x, y",
        returns: "integer",
        kind: FunctionKind::Scalar,
        min_args: 2,
        max_args: Some(2),
        summary: "Everywhere, two arguments.",
        since: None,
    },
];

#[test]
fn functions_are_filtered_by_server_version() {
    let mut d = pg().with_language(SqlLanguage {
        functions: VERSIONED,
        ..SqlLanguage::standard()
    });
    let c = ctx("SELECT <|>", &[Expected::Function], Clause::SelectList);
    assert_eq!(labels(&run(&c, &d, None)), ["NEW_FN", "OLD_FN"]);
    d.version = Some(ServerVersion::new(16, 2, 0));
    let list = run(&c, &d, None);
    assert_eq!(labels(&list), ["OLD_FN"]);
    // One item per name; overloads in the documentation.
    let doc = list.items[0].documentation.as_deref().unwrap();
    assert!(doc.contains("OLD_FN(x, y) → integer"), "{doc}");
}

#[test]
fn procedures_types_and_sequences() {
    let cache = cache(Engine::Postgres);
    let c = ctx("CALL ref<|>", &[Expected::Procedure], Clause::RoutineName);
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(labels(&list), ["refresh_stats"]);
    assert_eq!(list.items[0].kind, CompletionKind::Procedure);
    assert_eq!(list.items[0].insert_text, "refresh_stats");
    assert_eq!(
        list.items[0].detail.as_deref(),
        Some("refresh_stats(p_full boolean)")
    );

    let c = ctx(
        "SELECT CAST(x AS var<|>",
        &[Expected::Type],
        Clause::TypeName,
    );
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(list.items[0].label, "varchar");
    assert_eq!(list.items[0].insert_text, "varchar(${1:n})");
    assert!(list.items[0].is_snippet);
    let c = ctx(
        "SELECT CAST(x AS NUM<|>",
        &[Expected::Type],
        Clause::TypeName,
    );
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(list.items[0].insert_text, "NUMERIC(${1:p}, ${2:s})");
    let c = ctx(
        "SELECT CAST(x AS mo<|>",
        &[Expected::Type],
        Clause::TypeName,
    );
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(list.items[0].label, "mood");
    assert_eq!(list.items[0].detail.as_deref(), Some("type · public"));

    let c = ctx("SELECT nextval(<|>", &[Expected::Sequence], Clause::Other);
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(labels(&list), ["order_seq"]);
    assert_eq!(list.items[0].kind, CompletionKind::Sequence);

    let c = ctx("USE <|>", &[Expected::Database], Clause::Other);
    let list = run(&c, &pg(), Some(&cache));
    assert_eq!(labels(&list), ["shop", "archive"]);
    assert_eq!(list.items[0].detail.as_deref(), Some("current database"));
}

// --- limits, recency, cancellation ----------------------------------------------------------

#[test]
fn the_limit_marks_the_list_incomplete() {
    let big = SchemaCache::new(oxbase_schema_cache::synthetic::snapshot(
        &oxbase_schema_cache::synthetic::SyntheticSpec {
            tables: 500,
            columns_per_table: 5,
            routines: 10,
            ..oxbase_schema_cache::synthetic::SyntheticSpec::exit_check()
        },
    ));
    let c = ctx("SELECT * FROM <|>", FROM, Clause::From);
    let list = complete(&c, &pg(), Some(&big), &[], 10, &CancellationToken::new()).unwrap();
    assert_eq!(list.items.len(), 10);
    assert!(list.incomplete);
    assert_well_formed(&list, &c);
}

#[test]
fn recently_used_names_rank_first_among_equals() {
    let cache = cache(Engine::Postgres);
    let c = ctx("SELECT * FROM ord<|>", FROM, Clause::From);
    let mut recent = RecentNames::new(8);
    recent.touch("ORDER_TOTALS");
    let list = complete_with_recent(
        &c,
        &pg(),
        Some(&cache),
        &[],
        &recent,
        50,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(labels(&list)[..2], ["order_totals", "orders"]);
    assert_eq!(recent.rank("order_totals"), 0);
    assert_eq!(recent.rank("orders"), 2);
    // An exact match still wins over a recent prefix match.
    let c = ctx("SELECT * FROM orders<|>", FROM, Clause::From);
    recent.touch("orders_archive");
    let list = complete_with_recent(
        &c,
        &pg(),
        Some(&cache),
        &[],
        &recent,
        50,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(list.items[0].label, "orders");
    // The list keeps at most its capacity.
    for i in 0..40 {
        recent.touch(&format!("t{i}"));
    }
    assert!(recent.len() <= 10);
    assert_eq!(recent.rank("t39"), 0);
    assert_eq!(recent.rank("order_totals"), 2);
}

#[test]
fn recent_names_come_from_outlines() {
    let mut recent = RecentNames::default();
    assert_eq!(recent.len(), 0);
    let outline = Outline {
        range: 0..30,
        class: StatementClass::Select,
        queries: vec![],
        refs: vec![
            oxbase_insight::NameRef {
                parts: vec![ident("public"), ident("orders")],
                ranges: vec![14..20, 21..27],
                role: oxbase_insight::RefRole::Relation,
                query: Some(0),
                clause: Clause::From,
            },
            oxbase_insight::NameRef {
                parts: vec![ident("total")],
                ranges: std::iter::once(7..12).collect(),
                role: oxbase_insight::RefRole::Column,
                query: Some(0),
                clause: Clause::SelectList,
            },
            oxbase_insight::NameRef {
                parts: vec![ident("cust")],
                ranges: std::iter::once(30..34).collect(),
                role: oxbase_insight::RefRole::Relation,
                query: Some(0),
                clause: Clause::From,
            },
        ],
    };
    recent.record(&outline, 34);
    assert_eq!(recent.len(), 1);
    assert_eq!(recent.rank("Orders"), 0);
}

#[test]
fn cancellation_stops_the_request() {
    let token = CancellationToken::new();
    token.cancel();
    let c = ctx("SELECT * FROM <|>", FROM, Clause::From);
    let err = complete(&c, &pg(), Some(&cache(Engine::Postgres)), &[], 10, &token).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Cancelled);
}

#[test]
fn case_rules_come_from_the_cache() {
    // A case-sensitive SQL Server database: mixed case is still bare (no folding).
    let mut snap = snapshot(Engine::MsSql);
    snap.case_rules = CaseRules::uniform(IdentCase::Sensitive);
    let cache = SchemaCache::new(snap);
    let d = Dialect::builtin(Engine::MsSql).unwrap();
    let c = ctx("SELECT * FROM Aud<|>", FROM, Clause::From);
    let list = run(&c, &d, Some(&cache));
    assert_eq!(list.items[0].insert_text, "AuditLog");
    // Matching ignores case even then.
    let c = ctx("SELECT * FROM aud<|>", FROM, Clause::From);
    assert_eq!(run(&c, &d, Some(&cache)).items[0].label, "AuditLog");
}

#[test]
fn snippets_follow_the_engine() {
    let snippets = vec![
        Snippet {
            id: "u1".into(),
            name: "Mine".into(),
            prefix: "mine".into(),
            body: "SELECT 1;$0".into(),
            description: String::new(),
            engines: vec![Engine::MySql],
            builtin: false,
        },
        Snippet {
            id: "u2".into(),
            name: "Everywhere".into(),
            prefix: "mine2".into(),
            body: "SELECT 2;$0".into(),
            description: "Two.".into(),
            engines: vec![],
            builtin: false,
        },
    ];
    let c = ctx(
        "mi<|>",
        &[Expected::Keyword, Expected::Snippet],
        Clause::Start,
    );
    let only_snippets = |list: &CompletionList| -> Vec<String> {
        list.items
            .iter()
            .filter(|i| i.kind == CompletionKind::Snippet)
            .map(|i| i.label.clone())
            .collect()
    };
    let list = complete(&c, &pg(), None, &snippets, 50, &CancellationToken::new()).unwrap();
    assert_eq!(only_snippets(&list), ["mine2"]);
    assert_eq!(find(&list, "mine2").documentation.as_deref(), Some("Two."));
    let my = Dialect::builtin(Engine::MySql).unwrap();
    let list = complete(&c, &my, None, &snippets, 50, &CancellationToken::new()).unwrap();
    assert_eq!(only_snippets(&list), ["mine", "mine2"]);
}
