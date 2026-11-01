//! What a schema change means for one cache: which relations to read again, or everything
//! (`docs/m6-contracts.md` §3.5).
//!
//! Names in a [`SchemaChange`] are as the user wrote them. They are turned into catalog names
//! with the cache's case rules: the relations the name already resolves to (so a dropped or
//! altered table is read again under its catalog spelling, and disappears when it is gone),
//! plus the spelling the server stores for a new one (`CREATE TABLE Foo` is `foo` on
//! PostgreSQL). The catalog readers match a relation scope ignoring case, and
//! [`merge_scope`] adds whatever they return, so a name the server spells differently still
//! lands in the cache.

use oxbase_dialect::{CatalogScope, RelationName};
use oxbase_schema_cache::{NamePart, SchemaCache};
use oxbase_sql::{DdlObject, DdlVerb, SchemaChange};
use oxbase_types::CatalogSnapshot;

/// What to read again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) enum Refresh {
    #[default]
    Nothing,
    /// The whole database.
    Everything,
    /// These relations.
    Relations(Vec<RelationName>),
}

/// The refresh a list of changes calls for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Plan {
    pub refresh: Refresh,
    /// Other databases of the same profile a change named (`db.schema.table`): their caches
    /// are marked stale.
    pub other_databases: Vec<String>,
}

/// The stored spelling of a name part under `case` (quoted parts are exact).
fn stored(part: &NamePart, case: oxbase_types::IdentCase) -> String {
    if part.quoted {
        part.text.clone()
    } else {
        case.fold_unquoted(&part.text)
    }
}

/// The database a qualifier names when it is not a schema of this cache but one of the other
/// databases of the snapshot.
fn other_database(snap: &CatalogSnapshot, part: &NamePart) -> Option<String> {
    let rule = snap.case_rules.schemas;
    if rule.matches(&part.text, part.quoted, &snap.database) {
        return None;
    }
    snap.databases
        .iter()
        .find(|d| rule.matches(&part.text, part.quoted, d))
        .cloned()
}

/// Adds `target` to `out` once.
fn push(out: &mut Vec<RelationName>, schema: &str, name: String) {
    let target = RelationName {
        schema: Some(schema.to_owned()),
        name,
    };
    if !out.contains(&target) {
        out.push(target);
    }
}

/// The relations `name` (as written) stands for in `cache`, into `out`. `false` when the name
/// cannot be read (the caller refreshes everything).
fn targets(
    cache: &SchemaCache,
    name: &[NamePart],
    out: &mut Vec<RelationName>,
    others: &mut Vec<String>,
) -> bool {
    let snap = cache.snapshot();
    let rules = cache.case_rules();
    let (schema, last) = match name {
        [last] => (None, last),
        [schema, last] => (Some(schema), last),
        [database, schema, last] => {
            if let Some(other) = other_database(snap, database) {
                others.push(other);
                return true;
            }
            if !rules
                .schemas
                .matches(&database.text, database.quoted, &snap.database)
            {
                return false;
            }
            (Some(schema), last)
        }
        _ => return false,
    };
    let schemas: Vec<String> = if let Some(part) = schema {
        if let Some(index) = cache.resolve_schema(part) {
            vec![
                cache
                    .schema(index)
                    .map(|s| s.name.clone())
                    .unwrap_or_default(),
            ]
        } else {
            if let Some(other) = other_database(snap, part) {
                others.push(other);
                return true;
            }
            vec![stored(part, rules.schemas)]
        }
    } else {
        let mut schemas: Vec<String> = cache
            .resolve_relation(std::slice::from_ref(last))
            .into_iter()
            .filter_map(|id| cache.schema(id.schema).map(|s| s.name.clone()))
            .collect();
        let default = snap
            .default_schema
            .clone()
            .or_else(|| snap.search_path.first().cloned())
            .or_else(|| snap.schemas.first().map(|s| s.name.clone()));
        if let Some(default) = default
            && !schemas.contains(&default)
        {
            schemas.push(default);
        }
        schemas
    };
    for schema in &schemas {
        let qualified = [
            NamePart {
                text: schema.clone(),
                quoted: true,
            },
            last.clone(),
        ];
        for id in cache.resolve_relation(&qualified) {
            if let Some(rel) = cache.relation(id) {
                push(out, schema, rel.name.clone());
            }
        }
        push(out, schema, stored(last, rules.relations));
    }
    true
}

/// The refresh `changes` call for on the cache `current` of `database`. Without a cache in
/// memory (`None`) any change means "everything" (there is nothing to merge into).
pub(super) fn plan(current: Option<&SchemaCache>, changes: &[SchemaChange]) -> Plan {
    let mut out = Plan::default();
    if changes.is_empty() {
        out.refresh = Refresh::Everything;
        return out;
    }
    let mut relations = Vec::new();
    let mut full = false;
    // `ALTER TABLE s.t RENAME TO u`: the new name lives in the old one's schema.
    let mut qualifier: Option<Vec<NamePart>> = None;
    for change in changes {
        match change.object {
            // Indexes and triggers hold no cached names.
            DdlObject::Index | DdlObject::Trigger => {}
            DdlObject::Table | DdlObject::View if !change.name.is_empty() => {
                let Some(cache) = current else {
                    full = true;
                    continue;
                };
                let mut name = change.name.clone();
                if change.verb == DdlVerb::Rename
                    && name.len() == 1
                    && let Some(q) = &qualifier
                {
                    name.splice(0..0, q.iter().cloned());
                }
                qualifier = (name.len() > 1).then(|| name[..name.len() - 1].to_vec());
                if !targets(cache, &name, &mut relations, &mut out.other_databases) {
                    full = true;
                }
            }
            _ => full = true,
        }
    }
    out.refresh = if full {
        Refresh::Everything
    } else if relations.is_empty() {
        Refresh::Nothing
    } else {
        Refresh::Relations(relations)
    };
    out
}

/// The scope to merge a partial read with: `scope` plus every relation the reader returned
/// (it matches names ignoring case, so it may return another spelling than asked for).
pub(super) fn merge_scope(
    current: &SchemaCache,
    partial: &CatalogSnapshot,
    scope: &CatalogScope,
) -> CatalogScope {
    let mut out = scope.clone();
    if let Some(relations) = &mut out.relations {
        let default = current
            .snapshot()
            .default_schema
            .clone()
            .or_else(|| partial.default_schema.clone());
        // Relations without a schema live in the default one (`SchemaCache::merge`).
        for r in relations.iter_mut() {
            if r.schema.is_none() {
                r.schema.clone_from(&default);
            }
        }
        for schema in &partial.schemas {
            for rel in &schema.relations {
                push(relations, &schema.name, rel.name.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use oxbase_schema_cache::synthetic::{SyntheticSpec, snapshot};
    use oxbase_types::{
        CaseRules, CatalogRelation, CatalogSchema, Engine, IdentCase, RelationKind, ServerVersion,
    };

    use super::*;

    fn part(text: &str, quoted: bool) -> NamePart {
        NamePart {
            text: text.into(),
            quoted,
        }
    }

    fn change(verb: DdlVerb, object: DdlObject, name: &[NamePart]) -> SchemaChange {
        SchemaChange {
            verb,
            object,
            name: name.to_vec(),
        }
    }

    fn relation(name: &str) -> CatalogRelation {
        CatalogRelation {
            name: name.into(),
            kind: RelationKind::Table,
            columns: vec![],
            foreign_keys: vec![],
            comment: None,
        }
    }

    /// PostgreSQL: `public.orders`, `public.Mixed`, `sales.orders`; search path `public`.
    fn pg() -> SchemaCache {
        let mut snap = CatalogSnapshot::new(Engine::Postgres, ServerVersion::new(18, 0, 0), "app");
        snap.databases = vec!["app".into(), "other".into()];
        snap.default_schema = Some("public".into());
        snap.search_path = vec!["public".into()];
        let mut public = CatalogSchema::new("public");
        public.relations = vec![relation("orders"), relation("Mixed")];
        let mut sales = CatalogSchema::new("sales");
        sales.relations = vec![relation("orders")];
        snap.schemas = vec![public, sales];
        SchemaCache::new(snap)
    }

    fn rel(schema: &str, name: &str) -> RelationName {
        RelationName {
            schema: Some(schema.into()),
            name: name.into(),
        }
    }

    #[test]
    fn table_changes_refresh_their_relations() {
        let cache = pg();
        let p = plan(
            Some(&cache),
            &[change(
                DdlVerb::Alter,
                DdlObject::Table,
                &[part("ORDERS", false)],
            )],
        );
        assert_eq!(p.refresh, Refresh::Relations(vec![rel("public", "orders")]));

        let p = plan(
            Some(&cache),
            &[change(
                DdlVerb::Create,
                DdlObject::Table,
                &[part("Sales", false), part("New", false)],
            )],
        );
        assert_eq!(p.refresh, Refresh::Relations(vec![rel("sales", "new")]));

        // Quoted names keep their case; existing matches come first.
        let p = plan(
            Some(&cache),
            &[change(
                DdlVerb::Drop,
                DdlObject::Table,
                &[part("Mixed", true)],
            )],
        );
        assert_eq!(p.refresh, Refresh::Relations(vec![rel("public", "Mixed")]));
    }

    #[test]
    fn renames_inherit_the_schema_and_other_changes_refresh_everything() {
        let cache = pg();
        let p = plan(
            Some(&cache),
            &[
                change(
                    DdlVerb::Alter,
                    DdlObject::Table,
                    &[part("sales", false), part("orders", false)],
                ),
                change(DdlVerb::Rename, DdlObject::Table, &[part("orders2", false)]),
            ],
        );
        assert_eq!(
            p.refresh,
            Refresh::Relations(vec![rel("sales", "orders"), rel("sales", "orders2")])
        );
        let p = plan(
            Some(&cache),
            &[change(
                DdlVerb::Create,
                DdlObject::Function,
                &[part("f", false)],
            )],
        );
        assert_eq!(p.refresh, Refresh::Everything);
        assert_eq!(plan(Some(&cache), &[]).refresh, Refresh::Everything);
        let p = plan(
            Some(&cache),
            &[change(DdlVerb::Rename, DdlObject::Other, &[])],
        );
        assert_eq!(p.refresh, Refresh::Everything);
        let p = plan(
            Some(&cache),
            &[change(
                DdlVerb::Create,
                DdlObject::Index,
                &[part("i", false)],
            )],
        );
        assert_eq!(p.refresh, Refresh::Nothing);
        assert_eq!(
            plan(
                None,
                &[change(
                    DdlVerb::Create,
                    DdlObject::Table,
                    &[part("t", false)]
                )]
            )
            .refresh,
            Refresh::Everything
        );
    }

    #[test]
    fn other_databases_are_marked_not_refreshed() {
        let cache = pg();
        let p = plan(
            Some(&cache),
            &[change(
                DdlVerb::Drop,
                DdlObject::Table,
                &[
                    part("other", false),
                    part("public", false),
                    part("t", false),
                ],
            )],
        );
        assert_eq!(p.refresh, Refresh::Nothing);
        assert_eq!(p.other_databases, vec!["other".to_owned()]);
        let p = plan(
            Some(&cache),
            &[change(
                DdlVerb::Drop,
                DdlObject::Table,
                &[
                    part("app", false),
                    part("public", false),
                    part("orders", false),
                ],
            )],
        );
        assert_eq!(p.refresh, Refresh::Relations(vec![rel("public", "orders")]));
    }

    #[test]
    fn mysql_names_use_the_database_schema() {
        let mut snap = snapshot(&SyntheticSpec {
            engine: Engine::MySql,
            tables: 3,
            columns_per_table: 2,
            schemas: 1,
            routines: 0,
            ..SyntheticSpec::exit_check()
        });
        snap.case_rules = CaseRules {
            schemas: IdentCase::Sensitive,
            relations: IdentCase::Sensitive,
            columns: IdentCase::Insensitive,
            routines: IdentCase::Insensitive,
        };
        let db = snap.schemas[0].name.clone();
        snap.database.clone_from(&db);
        snap.default_schema = Some(db.clone());
        snap.databases = vec![db.clone(), "elsewhere".into()];
        let cache = SchemaCache::new(snap);
        let p = plan(
            Some(&cache),
            &[change(
                DdlVerb::Create,
                DdlObject::Table,
                &[part("NewOne", false)],
            )],
        );
        assert_eq!(p.refresh, Refresh::Relations(vec![rel(&db, "NewOne")]));
        let p = plan(
            Some(&cache),
            &[change(
                DdlVerb::Create,
                DdlObject::Table,
                &[part("elsewhere", false), part("t", false)],
            )],
        );
        assert_eq!(
            (p.refresh, p.other_databases),
            (Refresh::Nothing, vec!["elsewhere".into()])
        );
    }

    #[test]
    fn merge_scope_adds_what_the_reader_returned() {
        let cache = pg();
        let mut partial =
            CatalogSnapshot::new(Engine::Postgres, ServerVersion::new(18, 0, 0), "app");
        let mut public = CatalogSchema::new("public");
        public.relations = vec![relation("Orders")];
        partial.schemas = vec![public];
        let scope = CatalogScope {
            relations: Some(vec![
                RelationName {
                    schema: None,
                    name: "orders".into(),
                },
                rel("public", "gone"),
            ]),
            ..CatalogScope::full()
        };
        let merged = merge_scope(&cache, &partial, &scope);
        assert_eq!(
            merged.relations,
            Some(vec![
                rel("public", "orders"),
                rel("public", "gone"),
                rel("public", "Orders")
            ])
        );
        let next = cache.merge(partial, &merged);
        let names: Vec<&str> = next.snapshot().schemas[0]
            .relations
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(names, ["Mixed", "Orders"]);
    }
}
