//! Where the M2 seed objects of `tests/fixtures.toml` (`[m2]`, `[m2.<engine>]`) live in the
//! object tree, per object kind: the parent a `Fetcher::list` call gets and the names it must
//! return.
//!
//! The parents follow the fetcher parent convention of `docs/m2-contracts.md` §3.3: databases
//! under [`ObjectRef::root`], schemas under their database, schema-level kinds under the schema
//! (PostgreSQL) or the database (MySQL, MariaDB, SQLite), table children under their table.
//! Everything is derived from the fixtures file; the only names written here are the seed
//! table columns, which `tests/fixtures.toml` documents as part of the shape but does not list
//! as data ([`PARENT_COLUMNS`], [`CHILD_COLUMNS`]).
//!
//! Since M4 an engine with an `[m4.<engine>]` table (SQL Server) adds the groups of
//! [`m4_groups`]: the kinds its tree layout has beyond the M2 shape (types, view children).
//!
//! Since M7 MongoDB and Redis get their groups from `[m7.mongodb]` / `[m7.redis]`
//! ([`m7_mongo_groups`], [`m7_redis_groups`]), following the refs of `docs/m7-contracts.md` §3:
//! MongoDB collections, views and roles under their database, indexes under their collection,
//! the seed user under `admin`; Redis databases under the root, namespaces (`Schema`, the full
//! prefix with its separator) under their database or enclosing namespace, keys under their
//! namespace, ACL users under the root.

use std::collections::BTreeMap;

use oxbase_types::{ObjectKind, ObjectRef};

use crate::fixtures::{
    M2EngineSeed, M2Fixture, M4EngineSeed, M4Fixture, M7MongoSeed, M7RedisSeed, MONGO_USER_DATABASE,
};

/// Columns of the parent seed table (`[m2] tables[0]`), in ordinal order.
pub const PARENT_COLUMNS: [&str; 3] = ["id", "code", "note"];
/// Columns of the child seed table (`[m2] tables[1]`), in ordinal order.
pub const CHILD_COLUMNS: [&str; 3] = ["id", "parent_id", "qty"];

/// One `Fetcher::list` call of a `fetch_list` cell: the rows under `parent` must include every
/// name in `names` and none of `absent` (seed objects the tree must not show there, such as
/// the `[m4]` database-level DDL trigger under a table's or view's Triggers folder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedGroup {
    pub parent: ObjectRef,
    pub names: Vec<String>,
    pub absent: Vec<String>,
}

impl SeedGroup {
    fn new(parent: ObjectRef, names: Vec<String>) -> Option<Self> {
        (!names.is_empty()).then_some(Self {
            parent,
            names,
            absent: Vec::new(),
        })
    }
}

/// The list calls that check `kind` against the seed, or an empty list when the fixtures seed
/// nothing of that kind on this engine (the cell then stays pending).
pub fn seed_groups(m2: &M2Fixture, seed: &M2EngineSeed, kind: ObjectKind) -> Vec<SeedGroup> {
    let Some(db) = seed.database.as_deref().filter(|d| !d.is_empty()) else {
        return Vec::new();
    };
    let database = ObjectRef::database(db);
    // Where schema-level objects live: the schema on PostgreSQL, else the database.
    let container = match seed.schema.as_deref() {
        Some(schema) => database.child(ObjectKind::Schema, schema),
        None => database.clone(),
    };
    let on_engine = |name: &Option<String>| -> Vec<String> {
        name.iter()
            .filter(|n| seed.objects.contains(n))
            .cloned()
            .collect()
    };
    let parent_table = m2.tables.first().filter(|t| seed.objects.contains(t));
    let child_table = m2.tables.get(1).filter(|t| seed.objects.contains(t));
    let table = |name: &String| container.child(ObjectKind::Table, name.clone());
    let owned = |t: Option<&String>, names: Vec<String>| -> Option<SeedGroup> {
        SeedGroup::new(table(t?), names)
    };
    let groups: Vec<Option<SeedGroup>> = match kind {
        ObjectKind::Database => {
            let mut names = vec![db.to_owned()];
            names.extend(seed.other_database.iter().cloned());
            names.extend(seed.attached_as.iter().cloned());
            vec![SeedGroup::new(ObjectRef::root(), names)]
        }
        ObjectKind::Schema => vec![SeedGroup::new(
            database,
            seed.schema.iter().cloned().collect(),
        )],
        ObjectKind::Table => {
            let names = m2
                .tables
                .iter()
                .filter(|t| seed.objects.contains(t))
                .cloned()
                .collect();
            vec![SeedGroup::new(container.clone(), names)]
        }
        ObjectKind::View => vec![SeedGroup::new(container.clone(), on_engine(&m2.view))],
        ObjectKind::MatView => vec![SeedGroup::new(
            container.clone(),
            seed.matview.iter().cloned().collect(),
        )],
        ObjectKind::Function => {
            let mut names = on_engine(&m2.function);
            names.extend(seed.trigger_function.iter().cloned());
            vec![SeedGroup::new(container.clone(), names)]
        }
        ObjectKind::Procedure => vec![SeedGroup::new(container.clone(), on_engine(&m2.procedure))],
        ObjectKind::Sequence => vec![SeedGroup::new(container.clone(), on_engine(&m2.sequence))],
        ObjectKind::Trigger => {
            let on = m2
                .trigger_table
                .as_ref()
                .filter(|t| seed.objects.contains(t));
            vec![owned(on, on_engine(&m2.trigger))]
        }
        ObjectKind::Column => vec![
            owned(parent_table, owned_names(&PARENT_COLUMNS)),
            owned(child_table, owned_names(&CHILD_COLUMNS)),
        ],
        ObjectKind::Index => vec![owned(child_table, m2.index.iter().cloned().collect())],
        ObjectKind::ForeignKey => {
            vec![owned(child_table, m2.foreign_key.iter().cloned().collect())]
        }
        ObjectKind::Check => vec![
            owned(parent_table, m2.check_parent.iter().cloned().collect()),
            owned(child_table, m2.check_child.iter().cloned().collect()),
        ],
        _ => Vec::new(),
    };
    groups.into_iter().flatten().collect()
}

/// The extra list calls of the `[m4]` seed (`docs/m4-contracts.md` §5.3) on an engine with an
/// `[m4.<engine>]` table: what the M2 shape does not cover but the engine's tree layout has.
/// Types (alias and table types) under the `[m4]` schema, and the indexed view's index and
/// INSTEAD OF trigger under the view (`parent_kind` = view). Empty when the fixtures do not name
/// the database or the schema.
pub fn m4_groups(m4: &M4Fixture, seed: &M4EngineSeed, kind: ObjectKind) -> Vec<SeedGroup> {
    let (Some(db), Some(schema)) = (
        seed.database.as_deref().filter(|d| !d.is_empty()),
        m4.schema.as_deref().filter(|s| !s.is_empty()),
    ) else {
        return Vec::new();
    };
    let container = ObjectRef::database(db).child(ObjectKind::Schema, schema);
    let names = |items: &[&Option<String>]| -> Vec<String> {
        items.iter().filter_map(|n| (*n).clone()).collect()
    };
    let view = m4
        .indexed_view
        .as_ref()
        .map(|v| container.child(ObjectKind::View, v.clone()));
    let group = match kind {
        ObjectKind::Type => SeedGroup::new(container, names(&[&m4.alias_type, &m4.table_type])),
        ObjectKind::Index => view.and_then(|v| SeedGroup::new(v, names(&[&m4.indexed_view_index]))),
        ObjectKind::Trigger => view.and_then(|v| SeedGroup::new(v, names(&[&m4.view_trigger]))),
        _ => None,
    };
    group.into_iter().collect()
}

/// The `[m4]` seed objects no `list` of `kind` may return on an engine with an `[m4.<engine>]`
/// table: the database-level DDL trigger is in no Triggers folder (`docs/m4-contracts.md` §3).
pub fn m4_never_listed(m4: &M4Fixture, kind: ObjectKind) -> Vec<String> {
    match kind {
        ObjectKind::Trigger => m4.ddl_trigger.iter().cloned().collect(),
        _ => Vec::new(),
    }
}

/// The seed collection an index belongs to: the longest of `collections` that `index` starts
/// with, followed by `_` (`m7_docs_name_idx` belongs to `m7_docs`).
pub fn index_collection<'a>(collections: &'a [String], index: &str) -> Option<&'a str> {
    collections
        .iter()
        .filter(|c| {
            index
                .strip_prefix(c.as_str())
                .is_some_and(|rest| rest.len() > 1 && rest.starts_with('_'))
        })
        .max_by_key(|c| c.len())
        .map(String::as_str)
}

/// The list calls that check `kind` against the `[m7.mongodb]` seed (empty when it seeds
/// nothing of that kind or names no database).
pub fn m7_mongo_groups(seed: &M7MongoSeed, kind: ObjectKind) -> Vec<SeedGroup> {
    let Some(db) = seed.database.as_deref().filter(|d| !d.is_empty()) else {
        return Vec::new();
    };
    let database = ObjectRef::database(db);
    let one = |name: &Option<String>| -> Vec<String> { name.iter().cloned().collect() };
    let groups: Vec<Option<SeedGroup>> = match kind {
        ObjectKind::Database => vec![SeedGroup::new(ObjectRef::root(), vec![db.to_owned()])],
        ObjectKind::Table => vec![SeedGroup::new(database, seed.collections.clone())],
        ObjectKind::View => vec![SeedGroup::new(database, one(&seed.view))],
        ObjectKind::Role => vec![SeedGroup::new(database, one(&seed.role))],
        ObjectKind::User => vec![SeedGroup::new(
            ObjectRef::database(MONGO_USER_DATABASE),
            one(&seed.user),
        )],
        ObjectKind::Index => {
            let mut by_collection: BTreeMap<&str, Vec<String>> = BTreeMap::new();
            for index in &seed.indexes {
                if let Some(c) = index_collection(&seed.collections, index) {
                    by_collection.entry(c).or_default().push(index.clone());
                }
            }
            seed.collections
                .iter()
                .filter_map(|c| {
                    let names = by_collection.remove(c.as_str())?;
                    SeedGroup::new(database.child(ObjectKind::Table, c.clone()), names)
                })
                .map(Some)
                .collect()
        }
        _ => Vec::new(),
    };
    groups.into_iter().flatten().collect()
}

/// The namespaces of `key` from the outermost: every prefix that ends with `separator` and is
/// followed by more of the key (`m7:ns:a:1` → `m7:`, `m7:ns:`, `m7:ns:a:`). None for an empty
/// separator (the profile option turns namespaces off).
pub fn redis_namespaces<'a>(key: &'a str, separator: &str) -> Vec<&'a str> {
    if separator.is_empty() {
        return Vec::new();
    }
    key.match_indices(separator)
        .map(|(at, _)| at + separator.len())
        .filter(|&end| end < key.len())
        .map(|end| &key[..end])
        .collect()
}

/// The ref of the namespace node `chain.last()` (or the database itself for an empty chain):
/// each namespace is a `Schema` child of the one before it.
fn namespace_ref(database: &ObjectRef, chain: &[&str]) -> ObjectRef {
    chain.iter().fold(database.clone(), |parent, ns| {
        parent.child(ObjectKind::Schema, (*ns).to_owned())
    })
}

/// The list calls that check `kind` against the `[m7.redis]` seed: databases (the seed's and,
/// with `db1_key`, database `1`), the namespaces and keys of `keys` (the optional RedisJSON key
/// left out), and the ACL user. Empty when it seeds nothing of that kind.
pub fn m7_redis_groups(seed: &M7RedisSeed, kind: ObjectKind) -> Vec<SeedGroup> {
    let db = seed
        .database
        .as_deref()
        .filter(|d| !d.is_empty())
        .unwrap_or("0");
    let database = ObjectRef::database(db);
    let separator = seed.namespace_separator.as_deref().unwrap_or(":");
    // Parent namespace chain -> names listed under it (BTreeMap: a stable group order).
    let mut under: BTreeMap<Vec<&str>, Vec<String>> = BTreeMap::new();
    match kind {
        ObjectKind::Database => {
            let mut names = vec![db.to_owned()];
            if seed.db1_key.is_some() && db != "1" {
                names.push("1".to_owned());
            }
            return SeedGroup::new(ObjectRef::root(), names)
                .into_iter()
                .collect();
        }
        ObjectKind::User => {
            return SeedGroup::new(ObjectRef::root(), seed.acl_user.iter().cloned().collect())
                .into_iter()
                .collect();
        }
        ObjectKind::Key => {
            for key in &seed.keys {
                let chain = redis_namespaces(key, separator);
                let names = under.entry(chain).or_default();
                if !names.contains(key) {
                    names.push(key.clone());
                }
            }
        }
        ObjectKind::Schema => {
            for key in &seed.keys {
                let chain = redis_namespaces(key, separator);
                for (depth, ns) in chain.iter().enumerate() {
                    let names = under.entry(chain[..depth].to_vec()).or_default();
                    if !names.iter().any(|n| n == ns) {
                        names.push((*ns).to_owned());
                    }
                }
            }
        }
        _ => return Vec::new(),
    }
    under
        .into_iter()
        .filter_map(|(chain, names)| SeedGroup::new(namespace_ref(&database, &chain), names))
        .collect()
}

fn owned_names(names: &[&str]) -> Vec<String> {
    names.iter().map(|&n| n.to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::fixtures::{Fixtures, workspace_root};
    use oxbase_types::Engine;

    fn repo() -> Fixtures {
        Fixtures::from_path(&workspace_root().join("tests").join("fixtures.toml")).unwrap()
    }

    fn groups(fx: &Fixtures, engine: &str, kind: ObjectKind) -> Vec<(String, Vec<String>)> {
        let e: Engine = engine.parse().unwrap();
        let (m2, seed) = fx.m2_seed(engine, e).unwrap();
        seed_groups(m2, seed, kind)
            .into_iter()
            .map(|g| (g.parent.to_string(), g.names))
            .collect()
    }

    /// No groups, in the shape `groups` returns.
    const NONE: Vec<(String, Vec<String>)> = Vec::new();

    fn v(names: &[&str]) -> Vec<String> {
        owned_names(names)
    }

    #[test]
    fn postgres_seed_lives_in_schema_m2() {
        let fx = repo();
        let g = |k| groups(&fx, "postgres", k);
        assert_eq!(
            g(ObjectKind::Database),
            [(
                "database:".to_owned(),
                v(&["oxbase_test", "oxbase_m2_other"])
            )]
        );
        assert_eq!(
            g(ObjectKind::Schema),
            [("database:oxbase_test".to_owned(), v(&["m2"]))]
        );
        assert_eq!(
            g(ObjectKind::Table),
            [(
                "schema:oxbase_test.m2".to_owned(),
                v(&["m2_parent", "m2_child"])
            )]
        );
        assert_eq!(
            g(ObjectKind::Function),
            [(
                "schema:oxbase_test.m2".to_owned(),
                v(&["m2_fn", "m2_trg_fn"])
            )]
        );
        assert_eq!(
            g(ObjectKind::MatView),
            [("schema:oxbase_test.m2".to_owned(), v(&["m2_mview"]))]
        );
        assert_eq!(
            g(ObjectKind::Sequence),
            [("schema:oxbase_test.m2".to_owned(), v(&["m2_seq"]))]
        );
        assert_eq!(
            g(ObjectKind::Trigger),
            [("table:oxbase_test.m2.m2_child".to_owned(), v(&["m2_trg"]))]
        );
        assert_eq!(
            g(ObjectKind::Check),
            [
                (
                    "table:oxbase_test.m2.m2_parent".to_owned(),
                    v(&["m2_parent_code_chk"])
                ),
                (
                    "table:oxbase_test.m2.m2_child".to_owned(),
                    v(&["m2_child_qty_chk"])
                ),
            ]
        );
        assert_eq!(g(ObjectKind::Column).len(), 2);
        assert_eq!(g(ObjectKind::Extension), NONE);
    }

    #[test]
    fn mysql_seed_lives_in_the_database_and_has_no_sequence() {
        let fx = repo();
        let g = |k| groups(&fx, "mysql", k);
        assert_eq!(
            g(ObjectKind::Table),
            [(
                "database:oxbase_test".to_owned(),
                v(&["m2_parent", "m2_child"])
            )]
        );
        assert_eq!(
            g(ObjectKind::Index),
            [(
                "table:oxbase_test.m2_child".to_owned(),
                v(&["m2_child_parent_idx"])
            )]
        );
        assert_eq!(g(ObjectKind::Sequence), NONE);
        assert_eq!(g(ObjectKind::Schema), NONE);
        assert_eq!(g(ObjectKind::MatView), NONE);
        let maria = |k| groups(&fx, "mariadb", k);
        assert_eq!(
            maria(ObjectKind::Sequence),
            [("database:oxbase_test".to_owned(), v(&["m2_seq"]))]
        );
    }

    #[test]
    fn sqlite_seed_lists_main_and_the_attached_database() {
        let fx = repo();
        let g = |k| groups(&fx, "sqlite", k);
        assert_eq!(
            g(ObjectKind::Database),
            [("database:".to_owned(), v(&["main", "m2aux"]))]
        );
        assert_eq!(
            g(ObjectKind::Column),
            [
                (
                    "table:main.m2_parent".to_owned(),
                    v(&["id", "code", "note"])
                ),
                (
                    "table:main.m2_child".to_owned(),
                    v(&["id", "parent_id", "qty"])
                ),
            ]
        );
        for k in [
            ObjectKind::Function,
            ObjectKind::Procedure,
            ObjectKind::Sequence,
        ] {
            assert_eq!(g(k), NONE, "{k}");
        }
        assert_eq!(g(ObjectKind::View)[0].1, v(&["m2_view"]));
    }

    #[test]
    fn table_children_refs_follow_the_child_convention() {
        let fx = repo();
        let (m2, seed) = fx.m2_seed("postgres", Engine::Postgres).unwrap();
        let g = seed_groups(m2, seed, ObjectKind::ForeignKey);
        let want = ObjectRef::table(Some("oxbase_test"), Some("m2"), "m2_child");
        assert_eq!(g[0].parent, want);
        let fk = g[0]
            .parent
            .child(ObjectKind::ForeignKey, "m2_child_parent_fk");
        assert_eq!(fk.parent.as_deref(), Some("m2_child"));
        assert_eq!(fk.catalog.as_deref(), Some("oxbase_test"));
    }

    #[test]
    fn mssql_seed_lives_in_schema_m2_and_m4_adds_types_and_view_children() {
        let fx = repo();
        let g = |k| groups(&fx, "mssql", k);
        assert_eq!(
            g(ObjectKind::Database),
            [(
                "database:".to_owned(),
                v(&["oxbase_test", "oxbase_m2_other"])
            )]
        );
        assert_eq!(
            g(ObjectKind::Schema),
            [("database:oxbase_test".to_owned(), v(&["m2"]))]
        );
        assert_eq!(
            g(ObjectKind::Sequence),
            [("schema:oxbase_test.m2".to_owned(), v(&["m2_seq"]))]
        );
        assert_eq!(
            g(ObjectKind::Trigger),
            [("table:oxbase_test.m2.m2_child".to_owned(), v(&["m2_trg"]))]
        );
        assert_eq!(g(ObjectKind::MatView), NONE);

        let (m4, seed) = fx.m4_seed("mssql", Engine::MsSql).unwrap();
        let m4g = |k| -> Vec<(ObjectRef, Vec<String>)> {
            m4_groups(m4, &seed, k)
                .into_iter()
                .map(|g| (g.parent, g.names))
                .collect()
        };
        let schema = ObjectRef::database("oxbase_test").child(ObjectKind::Schema, "m4");
        assert_eq!(
            m4g(ObjectKind::Type),
            [(schema.clone(), v(&["m4_code_t", "m4_rows_t"]))]
        );
        let view = schema.child(ObjectKind::View, "m4_iview");
        assert_eq!(
            m4g(ObjectKind::Index),
            [(view.clone(), v(&["m4_iview_ix"]))]
        );
        assert_eq!(
            m4g(ObjectKind::Trigger),
            [(view.clone(), v(&["m4_view_trg"]))]
        );
        // View children say so; the ref a fetcher must list is parent.child(kind, name).
        let ix = view.child(ObjectKind::Index, "m4_iview_ix");
        assert_eq!(ix.parent_kind, Some(ObjectKind::View));
        assert_eq!(ix.parent.as_deref(), Some("m4_iview"));
        assert_eq!(ix.schema.as_deref(), Some("m4"));
        assert_eq!(
            m4g(ObjectKind::Table),
            Vec::<(ObjectRef, Vec<String>)>::new()
        );
        // The DDL trigger must stay out of every Triggers folder.
        assert_eq!(m4_never_listed(m4, ObjectKind::Trigger), v(&["m4_ddl_trg"]));
        assert_eq!(m4_never_listed(m4, ObjectKind::Index), Vec::<String>::new());
        // No [m4.<engine>] elsewhere.
        assert!(fx.m4_seed("postgres", Engine::Postgres).is_none());
    }

    #[test]
    fn m4_groups_need_a_database_and_a_schema() {
        let m4 = M4Fixture {
            alias_type: Some("t".to_owned()),
            ..M4Fixture::default()
        };
        let seed = M4EngineSeed {
            database: Some("db".to_owned()),
            ..M4EngineSeed::default()
        };
        assert_eq!(
            m4_groups(&m4, &seed, ObjectKind::Type),
            Vec::<SeedGroup>::new()
        );
        let m4 = M4Fixture {
            schema: Some("s".to_owned()),
            ..m4
        };
        assert_eq!(m4_groups(&m4, &seed, ObjectKind::Type).len(), 1);
        assert_eq!(
            m4_groups(&m4, &M4EngineSeed::default(), ObjectKind::Type),
            Vec::<SeedGroup>::new()
        );
        // No indexed view named: no view children.
        assert_eq!(
            m4_groups(&m4, &seed, ObjectKind::Index),
            Vec::<SeedGroup>::new()
        );
    }

    fn m7(fx: &Fixtures, engine: Engine, kind: ObjectKind) -> Vec<(String, Vec<String>)> {
        let groups = match engine {
            Engine::MongoDb => m7_mongo_groups(fx.m7_mongo(engine).unwrap(), kind),
            _ => m7_redis_groups(fx.m7_redis(engine).unwrap(), kind),
        };
        groups
            .into_iter()
            .map(|g| (g.parent.to_string(), g.names))
            .collect()
    }

    #[test]
    fn mongodb_seed_groups_follow_the_m7_tree() {
        let fx = repo();
        let g = |k| m7(&fx, Engine::MongoDb, k);
        assert_eq!(
            g(ObjectKind::Database),
            [("database:".to_owned(), v(&["oxbase_test"]))]
        );
        assert_eq!(
            g(ObjectKind::Table),
            [(
                "database:oxbase_test".to_owned(),
                v(&["m7_docs", "m7_big", "m7_edit"])
            )]
        );
        assert_eq!(
            g(ObjectKind::View),
            [("database:oxbase_test".to_owned(), v(&["m7_view"]))]
        );
        assert_eq!(
            g(ObjectKind::Index),
            [
                (
                    "table:oxbase_test.m7_docs".to_owned(),
                    v(&[
                        "m7_docs_name_idx",
                        "m7_docs_tags_idx",
                        "m7_docs_at_ttl",
                        "m7_docs_meta_city_uq"
                    ])
                ),
                ("table:oxbase_test.m7_big".to_owned(), v(&["m7_big_g_idx"])),
            ]
        );
        assert_eq!(
            g(ObjectKind::User),
            [("database:admin".to_owned(), v(&["m7_reader"]))]
        );
        assert_eq!(
            g(ObjectKind::Role),
            [("database:oxbase_test".to_owned(), v(&["m7_role"]))]
        );
        assert_eq!(g(ObjectKind::Column), NONE);
        // Index refs are children of their collection.
        let groups = m7_mongo_groups(fx.m7_mongo(Engine::MongoDb).unwrap(), ObjectKind::Index);
        let ix = groups[0]
            .parent
            .child(ObjectKind::Index, "m7_docs_name_idx");
        assert_eq!(ix.parent.as_deref(), Some("m7_docs"));
        assert_eq!(ix.catalog.as_deref(), Some("oxbase_test"));
        // Only the engine's own seed.
        assert!(fx.m7_mongo(Engine::Redis).is_none());
        assert!(fx.m7_redis(Engine::MongoDb).is_none());
    }

    #[test]
    fn redis_seed_groups_follow_the_namespace_tree() {
        let fx = repo();
        let g = |k| m7(&fx, Engine::Redis, k);
        assert_eq!(
            g(ObjectKind::Database),
            [("database:".to_owned(), v(&["0", "1"]))]
        );
        assert_eq!(
            g(ObjectKind::User),
            [("database:".to_owned(), v(&["m7_reader"]))]
        );
        assert_eq!(
            g(ObjectKind::Schema),
            [
                ("database:0".to_owned(), v(&["m7:"])),
                (
                    "schema:0.m7:".to_owned(),
                    v(&["m7:big:", "m7:ns:", "m7:edit:"])
                ),
                (
                    "schema:0.m7:.m7:ns:".to_owned(),
                    v(&["m7:ns:a:", "m7:ns:b:"])
                ),
            ]
        );
        let keys = g(ObjectKind::Key);
        assert_eq!(
            keys,
            [
                (
                    "schema:0.m7:".to_owned(),
                    v(&[
                        "m7:string",
                        "m7:binary",
                        "m7:hash",
                        "m7:list",
                        "m7:set",
                        "m7:zset",
                        "m7:stream",
                        "m7:ttl"
                    ])
                ),
                (
                    "schema:0.m7:.m7:big:".to_owned(),
                    v(&["m7:big:list", "m7:big:zset"])
                ),
                ("schema:0.m7:.m7:edit:".to_owned(), v(&["m7:edit:seed"])),
                ("schema:0.m7:.m7:ns:".to_owned(), v(&["m7:ns:leaf"])),
                (
                    "schema:0.m7:ns:.m7:ns:a:".to_owned(),
                    v(&["m7:ns:a:1", "m7:ns:a:2"])
                ),
                ("schema:0.m7:ns:.m7:ns:b:".to_owned(), v(&["m7:ns:b:1"])),
            ]
        );
        // Every seed key is in exactly one group; the optional RedisJSON key in none.
        let seed = fx.m7_redis(Engine::Redis).unwrap();
        let listed: usize = keys.iter().map(|(_, names)| names.len()).sum();
        assert_eq!(listed, seed.keys.len());
        assert!(keys.iter().all(|(_, n)| !n.contains(&"m7:json".to_owned())));
        // A key ref under a namespace: catalog = the database, schema = the namespace.
        let groups = m7_redis_groups(seed, ObjectKind::Key);
        let leaf = groups[3].parent.child(ObjectKind::Key, "m7:ns:leaf");
        assert_eq!(leaf.catalog.as_deref(), Some("0"));
        assert_eq!(leaf.schema.as_deref(), Some("m7:ns:"));
        assert_eq!(g(ObjectKind::Table), NONE);
    }

    #[test]
    fn redis_namespaces_and_index_owners() {
        assert_eq!(
            redis_namespaces("m7:ns:a:1", ":"),
            ["m7:", "m7:ns:", "m7:ns:a:"]
        );
        assert_eq!(redis_namespaces("plain", ":"), Vec::<&str>::new());
        // A trailing separator is part of the key's own name, not a namespace of it.
        assert_eq!(redis_namespaces("a:", ":"), Vec::<&str>::new());
        assert_eq!(redis_namespaces("a::b", "::"), ["a::"]);
        assert_eq!(redis_namespaces("m7:x", ""), Vec::<&str>::new());
        let colls = v(&["m7_docs", "m7_docs_x", "m7_big"]);
        assert_eq!(index_collection(&colls, "m7_docs_x_idx"), Some("m7_docs_x"));
        assert_eq!(
            index_collection(&colls, "m7_docs_name_idx"),
            Some("m7_docs")
        );
        assert_eq!(index_collection(&colls, "m7_docs_"), None);
        assert_eq!(index_collection(&colls, "other_idx"), None);
        // An empty separator: every key sits directly in the database.
        let seed = M7RedisSeed {
            keys: v(&["a:1", "b"]),
            namespace_separator: Some(String::new()),
            ..M7RedisSeed::default()
        };
        assert_eq!(
            m7_redis_groups(&seed, ObjectKind::Key)
                .into_iter()
                .map(|g| (g.parent.to_string(), g.names))
                .collect::<Vec<_>>(),
            [("database:0".to_owned(), v(&["a:1", "b"]))]
        );
        assert_eq!(
            m7_redis_groups(&seed, ObjectKind::Schema),
            Vec::<SeedGroup>::new()
        );
        assert_eq!(
            m7_mongo_groups(&M7MongoSeed::default(), ObjectKind::Table),
            Vec::<SeedGroup>::new()
        );
    }

    #[test]
    fn no_database_means_no_groups() {
        let m2 = M2Fixture {
            tables: v(&["a"]),
            ..M2Fixture::default()
        };
        let seed = M2EngineSeed {
            objects: v(&["a"]),
            ..M2EngineSeed::default()
        };
        assert_eq!(
            seed_groups(&m2, &seed, ObjectKind::Table),
            Vec::<SeedGroup>::new()
        );
    }
}
