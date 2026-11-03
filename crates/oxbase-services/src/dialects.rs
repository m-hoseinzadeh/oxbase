//! The dialect registry the app, the CLI and the conformance harness use (M2).

use oxbase_dialect::DialectRegistry;
use oxbase_types::{Engine, ObjectKind};

/// Builds the [`DialectRegistry`] with every engine's dialect cells.
///
/// Each engine crate registers itself through `oxbase_<engine>::dialect::register` (see
/// `DialectRegistry`'s *Registration* docs). Until an engine registers anything, its
/// not-applicable cells come from [`provisional_applicable`], the M0 table the conformance
/// harness used, so the object-kind matrix does not change shape. SQL Server registers its cells
/// since M4, MongoDB and Redis since M7 (`docs/m7-contracts.md` §1), so every shipped engine
/// registers its own cells now.
pub fn default_dialects() -> DialectRegistry {
    let mut reg = DialectRegistry::new();

    // M2 engine streams: each uncomments its own line (and only that line) when its `dialect`
    // module lands. Keep one blank line between them so the merges do not touch each other.

    oxbase_pg::dialect::register(&mut reg);

    oxbase_mysql::dialect::register(&mut reg);

    oxbase_sqlite::dialect::register(&mut reg);

    oxbase_mssql::dialect::register(&mut reg);

    // M7: fetchers, DocumentStore, BulkLoader and ScriptLanguage (the mongo shell).
    oxbase_mongo::dialect::register(&mut reg);

    // M7: fetchers, KeyValueStore, PubSub and ScriptLanguage (the console).
    oxbase_redis::dialect::register(&mut reg);

    // M8 (docs/m8-contracts.md §1): the transfer stream uncomments this line (and only this
    // line) once its TypeMapper cells are filled; until then `missing_m8_roles` reports them.
    oxbase_transfer::register(&mut reg);

    mark_provisional(&mut reg);
    reg
}

/// Marks the not-applicable cells of every engine that registered nothing, from
/// [`provisional_applicable`].
fn mark_provisional(reg: &mut DialectRegistry) {
    let registered = reg.engines();
    for engine in Engine::ALL {
        if registered.contains(&engine) {
            continue;
        }
        for kind in ObjectKind::ALL {
            if !provisional_applicable(engine, kind) {
                reg.mark_not_applicable(engine, kind);
            }
        }
    }
}

/// Whether `engine` has objects of `kind` at all, for engines whose dialect crate does not
/// register its own cells yet (version-independent; same table as the M0 conformance harness).
pub fn provisional_applicable(engine: Engine, kind: ObjectKind) -> bool {
    use ObjectKind as K;
    match engine {
        // A MySQL `SCHEMA` is a synonym for `DATABASE` (the `ObjectRef` catalog), so only
        // `database` gets a row.
        Engine::MySql => matches!(
            kind,
            K::Database
                | K::Table
                | K::Column
                | K::Index
                | K::ForeignKey
                | K::Check
                | K::View
                | K::Function
                | K::Procedure
                | K::Trigger
                | K::Event
                | K::User
                | K::Role
                | K::Grant
        ),
        // MariaDB adds sequences (10.3+) on top of the MySQL set.
        Engine::MariaDb => kind == K::Sequence || provisional_applicable(Engine::MySql, kind),
        Engine::Postgres => !matches!(kind, K::Event | K::Key),
        Engine::MsSql => !matches!(kind, K::MatView | K::Extension | K::Event | K::Key),
        Engine::Sqlite => matches!(
            kind,
            K::Database
                | K::Table
                | K::Column
                | K::Index
                | K::ForeignKey
                | K::Check
                | K::View
                | K::Trigger
        ),
        // A collection is `Table`.
        Engine::MongoDb => matches!(
            kind,
            K::Database | K::Table | K::Index | K::View | K::User | K::Role
        ),
        // Logical databases, keys and ACL users.
        Engine::Redis => matches!(kind, K::Database | K::Key | K::User),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_dialect::CellStatus;

    #[test]
    fn every_engine_has_applicability_marks() {
        let reg = default_dialects();
        for engine in Engine::ALL {
            for kind in ObjectKind::ALL {
                let applicable = reg.status(engine, kind) != CellStatus::NotApplicable;
                if reg.tree_layout(engine).is_err() {
                    // Still provisional.
                    assert_eq!(
                        applicable,
                        provisional_applicable(engine, kind),
                        "{engine}/{kind}"
                    );
                }
            }
        }
        assert!(!provisional_applicable(Engine::MySql, ObjectKind::Schema));
        assert!(provisional_applicable(
            Engine::MariaDb,
            ObjectKind::Sequence
        ));
        assert!(provisional_applicable(Engine::Sqlite, ObjectKind::Database));
    }

    /// M8 transfer: every SQL engine maps types (`docs/m8-contracts.md` §1); MongoDB and Redis
    /// are not transfer endpoints.
    #[test]
    fn every_sql_engine_has_a_type_mapper() {
        let reg = default_dialects();
        for engine in oxbase_dialect::M8_ENGINES {
            assert!(reg.has_type_mapper(engine), "{engine}");
            assert!(
                !reg.missing_m8_roles(engine)
                    .iter()
                    .any(|m| m.contains("TypeMapper")),
                "{engine}: {:?}",
                reg.missing_m8_roles(engine)
            );
        }
        assert!(!reg.has_type_mapper(Engine::MongoDb));
        assert!(!reg.has_type_mapper(Engine::Redis));
    }

    #[test]
    fn registered_engines_have_complete_layouts() {
        // Holds trivially until an engine registers; afterwards it guards the M2 contract that
        // every kind in a tree layout has a fetcher.
        let reg = default_dialects();
        for engine in Engine::ALL {
            assert_eq!(
                reg.missing_layout_fetchers(engine),
                Vec::<ObjectKind>::new(),
                "{engine}"
            );
        }
    }

    #[test]
    fn every_sql_engine_registers_its_native_bulk_loader() {
        // M5 integration (docs/m5-contracts.md §1, §4.4): no SQL engine falls back to the
        // generic `InsertLoader`.
        let reg = default_dialects();
        for engine in [
            Engine::Postgres,
            Engine::MySql,
            Engine::MariaDb,
            Engine::Sqlite,
            Engine::MsSql,
        ] {
            assert!(reg.has_bulk_loader(engine), "{engine}");
        }
        // M7: MongoDB imports through `insertMany`; Redis has no bulk loader.
        assert!(reg.has_bulk_loader(Engine::MongoDb));
        assert!(!reg.has_bulk_loader(Engine::Redis));
    }

    #[test]
    fn every_engine_registers_its_own_cells() {
        // M7 integration (docs/m7-contracts.md §1): no engine is provisional any more.
        let reg = default_dialects();
        for engine in Engine::ALL {
            assert!(reg.tree_layout(engine).is_ok(), "{engine}");
        }
    }

    #[test]
    fn document_and_key_value_engines_have_their_m7_roles() {
        // M7 integration (docs/m7-contracts.md §1): the tree, fetchers, script language and the
        // document / key-value / pub-sub roles of both engines.
        let reg = default_dialects();
        for engine in Engine::ALL {
            assert_eq!(
                reg.missing_m7_roles(engine),
                Vec::<String>::new(),
                "{engine}"
            );
        }
        for engine in [Engine::MongoDb, Engine::Redis] {
            assert_eq!(
                reg.missing_layout_fetchers(engine),
                Vec::<ObjectKind>::new(),
                "{engine}"
            );
        }
    }

    #[test]
    fn every_sql_engine_has_its_insight_roles() {
        // M6 integration (docs/m6-contracts.md §1): a catalog reader and language data that
        // passes `SqlLanguage::validate` for every SQL engine.
        let reg = default_dialects();
        for engine in [
            Engine::Postgres,
            Engine::MySql,
            Engine::MariaDb,
            Engine::Sqlite,
            Engine::MsSql,
        ] {
            assert_eq!(
                reg.missing_insight_roles(engine),
                Vec::<String>::new(),
                "{engine}"
            );
        }
    }
}
