//! The dialect registry the app, the CLI and the conformance harness use (M2).

use oxbase_dialect::DialectRegistry;
use oxbase_types::{Engine, ObjectKind};

/// Builds the [`DialectRegistry`] with every engine's dialect cells.
///
/// Each engine crate registers itself through `oxbase_<engine>::dialect::register` (see
/// `DialectRegistry`'s *Registration* docs). Until an engine registers anything, its
/// not-applicable cells come from [`provisional_applicable`], the M0 table the conformance
/// harness used, so the object-kind matrix does not change shape. SQL Server stays provisional
/// until its M4 line below is uncommented; MongoDB and Redis until M7.
pub fn default_dialects() -> DialectRegistry {
    let mut reg = DialectRegistry::new();

    // M2 engine streams: each uncomments its own line (and only that line) when its `dialect`
    // module lands. Keep one blank line between them so the merges do not touch each other.

    oxbase_pg::dialect::register(&mut reg);

    oxbase_mysql::dialect::register(&mut reg);

    oxbase_sqlite::dialect::register(&mut reg);

    // M4: uncommented at integration, once both the fetch and the ddl streams have filled the
    // SQL Server cells (registering it calls every cell, so no skeleton may remain).
    // oxbase_mssql::dialect::register(&mut reg);

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
}
