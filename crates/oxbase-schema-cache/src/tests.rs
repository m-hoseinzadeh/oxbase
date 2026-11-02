use super::*;
use oxbase_types::{CatalogColumn, IdentCase, RelationKind, ServerVersion};
use pretty_assertions::assert_eq;

const NONE: Vec<RelationId> = Vec::new();

fn part(text: &str) -> NamePart {
    NamePart {
        text: text.to_owned(),
        quoted: false,
    }
}

fn quoted(text: &str) -> NamePart {
    NamePart {
        text: text.to_owned(),
        quoted: true,
    }
}

fn rel(name: &str, columns: &[&str]) -> CatalogRelation {
    CatalogRelation {
        name: name.to_owned(),
        kind: RelationKind::Table,
        columns: columns
            .iter()
            .map(|c| CatalogColumn {
                name: (*c).to_owned(),
                native_type: "integer".to_owned(),
                nullable: true,
                primary_key: false,
            })
            .collect(),
        foreign_keys: vec![],
        comment: None,
    }
}

/// PostgreSQL-like: `public` (search path) and `sales`, with a mixed-case table.
fn pg() -> SchemaCache {
    let mut snap = CatalogSnapshot::new(Engine::Postgres, ServerVersion::new(18, 0, 0), "app");
    snap.databases = vec!["app".into(), "other".into()];
    snap.default_schema = Some("public".into());
    snap.search_path = vec!["public".into()];
    let mut public = CatalogSchema::new("public");
    public.relations = vec![
        rel("orders", &["id", "customer_id", "total"]),
        rel("Order Lines", &["id", "order_id"]),
        rel("customers", &["id", "name"]),
    ];
    public.routines = vec![CatalogRoutine {
        name: "order_total".into(),
        kind: oxbase_types::RoutineKind::Function,
        args: "id bigint".into(),
        returns: Some("numeric".into()),
    }];
    public.sequences = vec!["orders_id_seq".into()];
    let mut sales = CatalogSchema::new("sales");
    sales.relations = vec![rel("orders", &["id", "region"]), rel("quotas", &["id"])];
    snap.schemas = vec![sales, public];
    SchemaCache::new(snap)
}

fn names(cache: &SchemaCache, hits: &[NameHit]) -> Vec<String> {
    hits.iter()
        .map(|h| cache.name_of(h.target).to_owned())
        .collect()
}

#[test]
fn builds_a_normalized_index() {
    let cache = pg();
    // Normalized: `public` sorts before `sales`.
    assert_eq!(cache.schema(0).unwrap().name, "public");
    assert_eq!(cache.engine(), Engine::Postgres);
    assert!(cache.is_complete());
    assert_ne!(cache.generation(), pg().generation());
    assert_eq!(cache.search_schemas(), vec![0]);
}

#[test]
fn prefix_search_is_case_insensitive_and_ranked() {
    let cache = pg();
    let hits = cache.search(&NameQuery::prefix("ORD", &[NameKind::Relation]));
    assert_eq!(names(&cache, &hits), ["orders", "orders", "Order Lines"]);
    let hits = cache.search(&NameQuery::prefix("orders", &[]));
    // Exact matches first, then longer names.
    assert_eq!(hits[0].score, 1000);
    assert_eq!(names(&cache, &hits), ["orders", "orders", "orders_id_seq"]);
    // Schema filter.
    let q = NameQuery {
        schema: Some(1),
        ..NameQuery::prefix("", &[NameKind::Relation])
    };
    assert_eq!(names(&cache, &cache.search(&q)), ["orders", "quotas"]);
    // Limit.
    let q = NameQuery {
        limit: 2,
        ..NameQuery::prefix("", &[])
    };
    assert_eq!(cache.search(&q).len(), 2);
}

#[test]
fn column_search_within_one_relation() {
    let cache = pg();
    let orders = cache.resolve_relation(&[part("orders")])[0];
    let q = NameQuery {
        relation: Some(orders),
        ..NameQuery::prefix("", &[NameKind::Column])
    };
    assert_eq!(
        names(&cache, &cache.search(&q)),
        ["id", "total", "customer_id"]
    );
    let q = NameQuery {
        relation: Some(orders),
        mode: MatchMode::Fuzzy,
        ..NameQuery::prefix("cid", &[NameKind::Column])
    };
    assert_eq!(names(&cache, &cache.search(&q)), ["customer_id"]);
    let q = NameQuery {
        relation: Some(orders),
        ..NameQuery::prefix("", &[NameKind::Relation])
    };
    assert_eq!(cache.search(&q), Vec::<NameHit>::new());
}

#[test]
fn fuzzy_search_prefers_prefixes_and_word_starts() {
    let cache = pg();
    let q = NameQuery {
        mode: MatchMode::Fuzzy,
        ..NameQuery::prefix("ol", &[NameKind::Relation])
    };
    assert_eq!(names(&cache, &cache.search(&q)), ["Order Lines"]);
    assert!(fuzzy_score("ord", "orders") > fuzzy_score("ord", "customer_orders"));
    assert!(fuzzy_score("co", "customer_orders") > fuzzy_score("co", "discount"));
    assert_eq!(fuzzy_score("xyz", "orders"), None);
    assert_eq!(fuzzy_score("", "orders"), Some(1));
}

#[test]
fn resolution_follows_case_rules_and_search_path() {
    let cache = pg();
    // Unquoted folds to lower case.
    assert_eq!(cache.resolve_relation(&[part("ORDERS")]).len(), 1);
    assert_eq!(cache.resolve_relation(&[quoted("ORDERS")]), NONE);
    assert_eq!(cache.resolve_relation(&[quoted("Order Lines")]).len(), 1);
    assert_eq!(cache.resolve_relation(&[part("Order Lines")]), NONE);
    // Search path: `quotas` is only in `sales`.
    assert_eq!(cache.resolve_relation(&[part("quotas")]), NONE);
    assert_eq!(cache.relations_named(&part("quotas")).len(), 1);
    let sales_orders = cache.resolve_relation(&[part("sales"), part("orders")]);
    assert_eq!(sales_orders.len(), 1);
    assert_eq!(
        cache.relation(sales_orders[0]).unwrap().columns[1].name,
        "region"
    );
    // Database-qualified.
    assert_eq!(
        cache
            .resolve_relation(&[part("app"), part("public"), part("orders")])
            .len(),
        1
    );
    assert_eq!(
        cache.resolve_relation(&[part("nope"), part("public"), part("orders")]),
        NONE
    );
    assert_eq!(cache.resolve_schema(&part("SALES")), Some(1));
    assert_eq!(cache.resolve_routine(&[part("Order_Total")]).len(), 1);
    let orders = cache.resolve_relation(&[part("orders")])[0];
    assert_eq!(cache.resolve_column(orders, &part("TOTAL")), Some(2));
    assert_eq!(cache.resolve_column(orders, &quoted("TOTAL")), None);
}

#[test]
fn insensitive_engines_match_any_case() {
    let mut snap = pg().snapshot().clone();
    snap.case_rules = CaseRules::uniform(IdentCase::Insensitive);
    let cache = SchemaCache::new(snap);
    assert_eq!(cache.resolve_relation(&[quoted("ORDERS")]).len(), 1);
    assert_eq!(cache.resolve_relation(&[part("order lines")]).len(), 1);
}

#[test]
fn merge_replaces_relations_and_schemas() {
    let cache = pg();
    let mut partial = CatalogSnapshot::new(Engine::Postgres, ServerVersion::new(18, 0, 0), "app");
    partial.search_path = vec!["public".into()];
    partial.read_at_ms = 5;
    let mut public = CatalogSchema::new("public");
    public.relations = vec![rel("orders", &["id", "status"]), rel("fresh", &["x"])];
    partial.schemas = vec![public];
    let scope = CatalogScope {
        relations: Some(vec![
            RelationName {
                schema: Some("public".into()),
                name: "orders".into(),
            },
            RelationName {
                schema: None,
                name: "fresh".into(),
            },
            RelationName {
                schema: None,
                name: "customers".into(),
            },
        ]),
        ..CatalogScope::full()
    };
    let merged = cache.merge(partial, &scope);
    assert_ne!(merged.generation(), cache.generation());
    let orders = merged.resolve_relation(&[part("orders")])[0];
    assert_eq!(merged.relation(orders).unwrap().columns[1].name, "status");
    assert_eq!(merged.resolve_relation(&[part("fresh")]).len(), 1);
    // Named in the scope but absent from the read: dropped.
    assert_eq!(merged.resolve_relation(&[part("customers")]), NONE);
    // Untouched.
    assert_eq!(merged.resolve_relation(&[quoted("Order Lines")]).len(), 1);
    assert_eq!(merged.snapshot().read_at_ms, 5);
    // The old cache is unchanged.
    assert_eq!(cache.resolve_relation(&[part("customers")]).len(), 1);

    let scope = CatalogScope {
        schemas: Some(vec!["sales".into()]),
        ..CatalogScope::full()
    };
    let merged = merged.merge(
        CatalogSnapshot::new(Engine::Postgres, ServerVersion::new(18, 0, 0), "app"),
        &scope,
    );
    assert_eq!(merged.resolve_schema(&part("sales")), None);
    assert_eq!(merged.snapshot().schemas.len(), 1);
}

#[test]
fn codec_round_trips_and_rejects_other_formats() {
    let snap = pg().snapshot().clone();
    let bytes = codec::encode(&snap);
    assert_eq!(codec::decode(&bytes).unwrap(), snap);
    let mut newer = snap;
    newer.format += 1;
    let err = codec::decode(&codec::encode(&newer)).unwrap_err();
    assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);
    assert!(codec::decode(b"not json").is_err());
}

#[test]
fn synthetic_catalogs_are_deterministic_and_sized() {
    let spec = synthetic::SyntheticSpec {
        tables: 300,
        columns_per_table: 12,
        routines: 40,
        ..synthetic::SyntheticSpec::exit_check()
    };
    let a = synthetic::snapshot(&spec);
    assert_eq!(a, synthetic::snapshot(&spec));
    assert_eq!(a.relation_count(), 300);
    assert_eq!(a.column_count(), 300 * 12);
    assert_eq!(a.routine_count(), 40);
    assert_eq!(a.schemas.len(), 4);
    for r in a.schemas.iter().flat_map(|s| &s.relations) {
        let mut cols: Vec<&str> = r.columns.iter().map(|c| c.name.as_str()).collect();
        cols.sort_unstable();
        cols.dedup();
        assert_eq!(cols.len(), 12, "duplicate column in {}", r.name);
    }
    // Foreign keys point at relations of the same schema.
    let mut fks = 0;
    for schema in &a.schemas {
        for r in &schema.relations {
            for fk in &r.foreign_keys {
                fks += 1;
                assert!(
                    schema.relations.iter().any(|x| x.name == fk.ref_table),
                    "{} -> {}",
                    r.name,
                    fk.ref_table
                );
            }
        }
    }
    assert!(fks > 300, "{fks} foreign keys");
    let my = synthetic::snapshot(&synthetic::SyntheticSpec {
        engine: Engine::MySql,
        ..spec
    });
    assert_eq!(my.schemas.len(), 1);
    assert_eq!(my.schemas[0].name, "app");
}

/// The memory budget of the crate docs, on the exit-check catalog. Fast enough for debug
/// builds (about 105 000 names).
#[test]
fn the_exit_check_catalog_fits_the_memory_budget() {
    let cache = SchemaCache::new(synthetic::snapshot(&synthetic::SyntheticSpec::exit_check()));
    assert_eq!(cache.snapshot().relation_count(), 5_000);
    assert_eq!(cache.snapshot().column_count(), 100_000);
    let bytes = cache.heap_bytes();
    assert!(bytes < MEMORY_BUDGET_BYTES, "{bytes} bytes");
    assert!(bytes > 4 * 1024 * 1024, "estimate too small: {bytes}");
    let hits = cache.search(&NameQuery::prefix("customer_", &[NameKind::Relation]));
    assert_eq!(hits.len(), 100);
}
