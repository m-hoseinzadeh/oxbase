//! Bridges the harness to the shared contracts in `oxbase-types`, `oxbase-dialect` and
//! `oxbase-services`.
//!
//! The harness uses [`oxbase_types::Engine`] and [`oxbase_types::ObjectKind`] directly, so its matrix
//! rows and columns are the same ones the dialect registry reports. A MongoDB collection is
//! [`ObjectKind::Table`] and a Redis key is [`ObjectKind::Key`], as documented in `oxbase-types`.
//!
//! Since M2 the registry is [`oxbase_services::default_dialects`], the one the app and the CLI
//! use: every engine crate that registered its dialect cells there brings its own fetchers and
//! not-applicable marks, and engines that registered nothing keep the provisional M0 table. A
//! cell is "not applicable" exactly when [`DialectRegistry::status`] says
//! [`DialectCellStatus::NotApplicable`].

use oxbase_dialect::{CellStatus as DialectCellStatus, DialectRegistry};

pub use oxbase_types::{Engine, ObjectKind};

/// Parses a fixture `kind` value: exactly one of the [`Engine::as_str`] names, ignoring case
/// and surrounding whitespace. Aliases accepted by `Engine::from_str` (`pg`, `mongo`, ...) are
/// rejected on purpose, so `fixtures.toml` stays canonical.
pub fn engine_from_fixture_kind(kind: &str) -> Option<Engine> {
    let kind = kind.trim();
    Engine::ALL
        .into_iter()
        .find(|e| e.as_str().eq_ignore_ascii_case(kind))
}

/// The dialect registry the harness reads applicability and fetchers from:
/// [`oxbase_services::default_dialects`].
pub fn registry() -> DialectRegistry {
    oxbase_services::default_dialects()
}

/// Whether `engine` has objects of `kind` at all (provisional, version-independent): the M0
/// table, which `oxbase_services::default_dialects` also applies to engines that registered
/// nothing. Unit tests use it through [`provisional_registry`] so their cell counts do not
/// depend on which engine crates have registered.
pub fn supports_kind(engine: Engine, kind: ObjectKind) -> bool {
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
        Engine::MariaDb => kind == K::Sequence || supports_kind(Engine::MySql, kind),
        Engine::Postgres => !matches!(kind, K::Event | K::Key),
        Engine::MsSql => !matches!(kind, K::MatView | K::Extension | K::Event | K::Key),
        // `main`, `temp` and attached databases are `Database` nodes (M2 tree).
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

/// A registry with no implementations, only the not-applicable marks of [`supports_kind`].
pub fn provisional_registry() -> DialectRegistry {
    let mut reg = DialectRegistry::new();
    for engine in Engine::ALL {
        for kind in ObjectKind::ALL {
            if !supports_kind(engine, kind) {
                reg.mark_not_applicable(engine, kind);
            }
        }
    }
    reg
}

/// Whether the (engine, kind) cell applies according to `registry`.
pub fn applies(registry: &DialectRegistry, engine: Engine, kind: ObjectKind) -> bool {
    registry.status(engine, kind) != DialectCellStatus::NotApplicable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_kind_round_trips() {
        for engine in Engine::ALL {
            assert_eq!(engine_from_fixture_kind(engine.as_str()), Some(engine));
        }
        assert_eq!(engine_from_fixture_kind(" MariaDB "), Some(Engine::MariaDb));
        assert_eq!(engine_from_fixture_kind("oracle"), None);
        // Aliases are not fixture kinds.
        assert_eq!(engine_from_fixture_kind("pg"), None);
    }

    #[test]
    fn every_engine_has_some_kind_and_every_kind_some_engine() {
        for engine in Engine::ALL {
            assert!(ObjectKind::ALL.iter().any(|&k| supports_kind(engine, k)));
        }
        for kind in ObjectKind::ALL {
            assert!(
                Engine::ALL.iter().any(|&e| supports_kind(e, kind)),
                "{kind}"
            );
        }
    }

    #[test]
    fn spot_check_applicability() {
        assert!(supports_kind(Engine::Postgres, ObjectKind::Extension));
        assert!(!supports_kind(Engine::MySql, ObjectKind::Sequence));
        assert!(!supports_kind(Engine::MySql, ObjectKind::Schema));
        assert!(supports_kind(Engine::MariaDb, ObjectKind::Sequence));
        assert!(supports_kind(Engine::MariaDb, ObjectKind::Event));
        assert!(!supports_kind(Engine::Sqlite, ObjectKind::User));
        assert!(supports_kind(Engine::Sqlite, ObjectKind::Database));
        assert!(supports_kind(Engine::MongoDb, ObjectKind::Table));
        assert!(supports_kind(Engine::Redis, ObjectKind::Key));
        assert!(!supports_kind(Engine::Redis, ObjectKind::Table));
    }

    #[test]
    fn provisional_registry_agrees_with_table() {
        let reg = provisional_registry();
        for engine in Engine::ALL {
            for kind in ObjectKind::ALL {
                assert_eq!(
                    applies(&reg, engine, kind),
                    supports_kind(engine, kind),
                    "{engine}/{kind}"
                );
            }
        }
        // Applicable cells are empty (no implementations), so every role of every applicable
        // cell counts as missing, and nothing else does.
        let applicable = Engine::ALL
            .iter()
            .flat_map(|&e| {
                ObjectKind::ALL
                    .iter()
                    .filter(move |&&k| supports_kind(e, k))
            })
            .count();
        let missing = reg.missing_cells(&Engine::ALL);
        assert_eq!(
            missing.len(),
            applicable * oxbase_dialect::CellRole::ALL.len()
        );
        assert!(missing.iter().all(|m| supports_kind(m.engine, m.kind)));
    }

    /// Engines that registered nothing in `default_dialects` keep the provisional table; the
    /// ones that did bring their own marks.
    #[test]
    fn default_registry_is_provisional_for_unregistered_engines() {
        let reg = registry();
        for engine in Engine::ALL {
            if reg.tree_layout(engine).is_ok() {
                continue;
            }
            for kind in ObjectKind::ALL {
                assert_eq!(
                    applies(&reg, engine, kind),
                    supports_kind(engine, kind),
                    "{engine}/{kind}"
                );
            }
        }
        // MongoDB and Redis register nothing before M7.
        assert!(!applies(&reg, Engine::Redis, ObjectKind::Table));
    }
}
