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

use oxbase_types::{ObjectKind, ObjectRef};

use crate::fixtures::{M2EngineSeed, M2Fixture};

/// Columns of the parent seed table (`[m2] tables[0]`), in ordinal order.
pub const PARENT_COLUMNS: [&str; 3] = ["id", "code", "note"];
/// Columns of the child seed table (`[m2] tables[1]`), in ordinal order.
pub const CHILD_COLUMNS: [&str; 3] = ["id", "parent_id", "qty"];

/// One `Fetcher::list` call of a `fetch_list` cell: the rows under `parent` must include every
/// name in `names`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedGroup {
    pub parent: ObjectRef,
    pub names: Vec<String>,
}

impl SeedGroup {
    fn new(parent: ObjectRef, names: Vec<String>) -> Option<Self> {
        (!names.is_empty()).then_some(Self { parent, names })
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
        assert!(g(ObjectKind::Extension).is_empty());
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
        assert!(g(ObjectKind::Sequence).is_empty());
        assert!(g(ObjectKind::Schema).is_empty());
        assert!(g(ObjectKind::MatView).is_empty());
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
            assert!(g(k).is_empty(), "{k}");
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
    fn no_database_means_no_groups() {
        let m2 = M2Fixture {
            tables: v(&["a"]),
            ..M2Fixture::default()
        };
        let seed = M2EngineSeed {
            objects: v(&["a"]),
            ..M2EngineSeed::default()
        };
        assert!(seed_groups(&m2, &seed, ObjectKind::Table).is_empty());
    }
}
