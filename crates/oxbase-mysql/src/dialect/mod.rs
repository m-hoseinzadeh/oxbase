//! MySQL and MariaDB dialect cells: object-tree fetchers, the tree layout, capabilities and
//! quoting (M2), and the designer and grid-editing roles (M3), for [`Engine::MySql`] and
//! [`Engine::MariaDb`] alike. [`register`] adds them to a [`DialectRegistry`].
//!
//! # M3 roles
//!
//! For `Table`, `Column`, `Index`, `ForeignKey` and `Check`: a DDL generator (module `ddl`: one
//! `CREATE TABLE` / `ALTER TABLE` with many clauses, every step non-transactional because each
//! DDL statement commits on its own) and a comparator (module `compare`: the shared walk of
//! `oxbase_dialect::compare` with MySQL's spelling rules, after removing what these servers do
//! not store, such as primary key names). Per engine: the DML writer (module `dml`: bound `?`
//! parameters, `LAST_INSERT_ID()` / MariaDB `RETURNING` read-back) and
//! `Capabilities::designer` (module `caps`), all version-aware through `ServerVersion`.
//!
//! # Catalog
//!
//! Everything is read from `information_schema` with the object names as **bound parameters**;
//! only fixed SQL text (version-specific column lists) is ever concatenated. `SHOW CREATE ...`
//! is not used, since it needs the name inside the statement: the `definition` of routines,
//! triggers and events is assembled from catalog columns in the layout `SHOW CREATE` prints,
//! around the server's own body text.
//!
//! A database is what other engines call a schema ([`DatabaseScope::Catalog`]): one session sees
//! every database, `ObjectRef::catalog` names it and every listed ref has it set. A ref without
//! a catalog means the session's current database (`DATABASE()`).
//!
//! # Kinds
//!
//! | Kind | List under | Source | `load` returns |
//! |---|---|---|---|
//! | Database | root | `SCHEMATA` | `Schema` (charset, collation; MariaDB 10.5+ comment) |
//! | Table | database | `TABLES` (base, system-versioned, temporary) | `Table` with columns, PK, indexes, FKs, checks |
//! | Column | table, view | `COLUMNS` | `Column` |
//! | Index | table | `STATISTICS` (the PK is `PRIMARY`, flagged `PRIMARY_KEY`) | `Index` |
//! | ForeignKey | table | `REFERENTIAL_CONSTRAINTS` + `KEY_COLUMN_USAGE` | `ForeignKey` |
//! | Check | table | `CHECK_CONSTRAINTS` (MySQL 8.0.16+, MariaDB 10.2.1+) | `Check` |
//! | View | database | `TABLES` + `VIEWS` | `View` |
//! | Function, Procedure | database | `ROUTINES` + `PARAMETERS` | `Routine` |
//! | Trigger | table | `TRIGGERS` | `Trigger` |
//! | Sequence | database (MariaDB 10.3+) | `TABLES`; details from `SEQUENCES` (MariaDB 11.5+) | `Sequence` |
//! | Event | database (not in the M2 tree layout) | `EVENTS` | `Generic` |
//!
//! MariaDB differences handled in the cells: `JSON` is an alias of `LONGTEXT` with an automatic
//! `CHECK (json_valid(col))`, so such a column is typed `Json { binary: false }` and that check
//! is not listed; check constraints are per table (MySQL: per database); column defaults are
//! already SQL expressions; sequences exist; index visibility is `IGNORED` (10.6+).
//!
//! System databases (`information_schema`, `mysql`, `performance_schema`, `sys`) and everything
//! in them are listed with `system = true`.

mod caps;
mod coltype;
mod compare;
mod database;
mod ddl;
mod ddl_text;
mod dml;
mod event;
mod expr;
mod native;
mod routine;
mod rows;
mod sequence;
mod table;
mod trigger;
mod view;

use std::sync::Arc;

use oxbase_dialect::{
    BacktickQuoting, Capabilities, ChildGroup, DESIGNER_KINDS, DatabaseScope, DesignerCapabilities,
    DialectRegistry, TreeLayout,
};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    Engine, Error, ObjectKind, ObjectRef, ObjectSummary, PropId, Result, ServerVersion, props,
};

use rows::query;

/// Extra: the column's `ON UPDATE` expression (`CURRENT_TIMESTAMP`).
pub const ON_UPDATE_EXPRESSION: PropId = PropId::from_static("on_update_expression");
/// Extra: an invisible column or index (MySQL `INVISIBLE`, MariaDB `IGNORED` index).
pub const INVISIBLE: PropId = PropId::from_static("invisible");
/// Extra: a check constraint is enforced (MySQL `ENFORCED`).
pub const ENFORCED: PropId = PropId::from_static("enforced");
/// Extra: a view is updatable (`IS_UPDATABLE`).
pub const UPDATABLE: PropId = PropId::from_static("updatable");
/// Extra: trigger order among triggers with the same timing and event (`ACTION_ORDER`).
pub const ACTION_ORDER: PropId = PropId::from_static("action_order");
/// Extra: event status (`ENABLED`, `DISABLED`, ...).
pub const STATUS: PropId = PropId::from_static("status");

/// The databases that belong to the server itself.
const SYSTEM_DATABASES: [&str; 4] = ["information_schema", "mysql", "performance_schema", "sys"];

/// Registers the MySQL and MariaDB cells: a fetcher for every kind of the tree layout (plus
/// events), the layout itself, capabilities, quoting and the not-applicable kinds (M2); a DDL
/// generator and a comparator for every designer kind, and the DML writer (M3).
pub fn register(reg: &mut DialectRegistry) {
    for engine in [Engine::MySql, Engine::MariaDb] {
        let mariadb = engine == Engine::MariaDb;
        for kind in DESIGNER_KINDS {
            reg.register_ddl(engine, kind, Arc::new(ddl::MySqlDdl { mariadb }));
            reg.register_comparator(
                engine,
                kind,
                Arc::new(compare::MySqlComparator {
                    rules: compare::MySqlRules { mariadb },
                }),
            );
        }
        reg.register_dml_writer(engine, Arc::new(dml::MySqlDmlWriter { mariadb }));
        reg.register_fetcher(
            engine,
            ObjectKind::Database,
            Arc::new(database::DatabaseFetcher),
        );
        reg.register_fetcher(engine, ObjectKind::Table, Arc::new(table::TableFetcher));
        reg.register_fetcher(engine, ObjectKind::Column, Arc::new(table::ColumnFetcher));
        reg.register_fetcher(engine, ObjectKind::Index, Arc::new(table::IndexFetcher));
        reg.register_fetcher(
            engine,
            ObjectKind::ForeignKey,
            Arc::new(table::ForeignKeyFetcher),
        );
        reg.register_fetcher(engine, ObjectKind::Check, Arc::new(table::CheckFetcher));
        reg.register_fetcher(engine, ObjectKind::View, Arc::new(view::ViewFetcher));
        reg.register_fetcher(
            engine,
            ObjectKind::Function,
            Arc::new(routine::RoutineFetcher::new(ObjectKind::Function)),
        );
        reg.register_fetcher(
            engine,
            ObjectKind::Procedure,
            Arc::new(routine::RoutineFetcher::new(ObjectKind::Procedure)),
        );
        reg.register_fetcher(
            engine,
            ObjectKind::Trigger,
            Arc::new(trigger::TriggerFetcher),
        );
        reg.register_fetcher(engine, ObjectKind::Event, Arc::new(event::EventFetcher));
        if engine == Engine::MariaDb {
            reg.register_fetcher(
                engine,
                ObjectKind::Sequence,
                Arc::new(sequence::SequenceFetcher),
            );
        }
        reg.register_tree_layout(engine, tree_layout(engine));
        reg.register_capabilities(engine, Arc::new(MySqlCapabilities::new(engine)));
        reg.register_quoting(engine, Arc::new(BacktickQuoting));
        reg.mark_not_applicable_except(engine, applicable_kinds(engine));
    }
}

/// The kinds that exist on `engine` at all (any version).
fn applicable_kinds(engine: Engine) -> &'static [ObjectKind] {
    use ObjectKind as K;
    const MYSQL: &[ObjectKind] = &[
        K::Database,
        K::Table,
        K::Column,
        K::Index,
        K::ForeignKey,
        K::Check,
        K::View,
        K::Function,
        K::Procedure,
        K::Trigger,
        K::Event,
        K::User,
        K::Role,
        K::Grant,
    ];
    const MARIADB: &[ObjectKind] = &[
        K::Database,
        K::Table,
        K::Column,
        K::Index,
        K::ForeignKey,
        K::Check,
        K::View,
        K::Function,
        K::Procedure,
        K::Trigger,
        K::Sequence,
        K::Event,
        K::User,
        K::Role,
        K::Grant,
    ];
    if engine == Engine::MariaDb {
        MARIADB
    } else {
        MYSQL
    }
}

/// The M2 object tree of MySQL / MariaDB (see `oxbase_dialect::TreeLayout`).
fn tree_layout(engine: Engine) -> TreeLayout {
    use ObjectKind as K;
    let mut database = vec![
        ChildGroup::folder(K::Table),
        ChildGroup::folder(K::View),
        ChildGroup::folder(K::Function),
        ChildGroup::folder(K::Procedure),
    ];
    if engine == Engine::MariaDb {
        database.push(ChildGroup::folder(K::Sequence));
    }
    TreeLayout::new(DatabaseScope::Catalog)
        .with_root(&[ChildGroup::direct(K::Database)])
        .with_children(K::Database, &database)
        .with_children(
            K::Table,
            &[
                ChildGroup::folder(K::Column),
                ChildGroup::folder(K::Index),
                ChildGroup::folder(K::ForeignKey),
                ChildGroup::folder(K::Check),
                ChildGroup::folder(K::Trigger),
            ],
        )
        .with_children(K::View, &[ChildGroup::folder(K::Column)])
}

/// What MySQL and MariaDB support, by server version.
#[derive(Debug, Clone, Copy)]
struct MySqlCapabilities {
    mariadb: bool,
}

impl MySqlCapabilities {
    fn new(engine: Engine) -> Self {
        Self {
            mariadb: engine == Engine::MariaDb,
        }
    }
}

/// Extras no MySQL or MariaDB object has.
const NEVER: [PropId; 16] = [
    props::UNLOGGED,
    // Session temporary tables are not listed (see `table::TABLE_TYPES`).
    props::TEMPORARY,
    props::WITHOUT_ROWID,
    props::STRICT,
    props::DEFERRABLE,
    props::INITIALLY_DEFERRED,
    props::CLUSTERED,
    props::INCLUDE_COLUMNS,
    props::PREDICATE,
    props::VOLATILITY,
    props::STRICT_INPUT,
    props::POPULATED,
    props::ENCODING,
    props::FILE,
    props::IDENTITY,
    props::FILL_FACTOR,
];

impl Capabilities for MySqlCapabilities {
    fn supports(&self, kind: ObjectKind, prop: PropId, v: &ServerVersion) -> bool {
        use ObjectKind as K;
        if !self.supports_kind(kind, v) || NEVER.contains(&prop) {
            return false;
        }
        if prop == props::VIEW_ALGORITHM || prop == props::CHECK_OPTION || prop == UPDATABLE {
            return kind == K::View;
        }
        if prop == props::DETERMINISTIC || prop == props::SQL_DATA_ACCESS {
            return matches!(kind, K::Function | K::Procedure);
        }
        if prop == props::STORAGE_ENGINE || prop == props::ROW_FORMAT {
            return kind == K::Table;
        }
        if prop == props::COMMENT && kind == K::Database {
            // `SCHEMATA.SCHEMA_COMMENT`: MariaDB 10.5+ only; MySQL has no database comments.
            return self.mariadb && v.at_least(10, 5);
        }
        if prop == INVISIBLE {
            return match kind {
                K::Column => {
                    if self.mariadb {
                        v.at_least(10, 3)
                    } else {
                        v.at_least_patch(8, 0, 23)
                    }
                }
                K::Index => {
                    if self.mariadb {
                        v.at_least(10, 6)
                    } else {
                        v.at_least(8, 0)
                    }
                }
                _ => false,
            };
        }
        if prop == ENFORCED {
            return kind == K::Check && !self.mariadb;
        }
        true
    }

    fn designer(&self, v: &ServerVersion) -> DesignerCapabilities {
        caps::designer(self.mariadb, v)
    }

    fn supports_kind(&self, kind: ObjectKind, v: &ServerVersion) -> bool {
        use ObjectKind as K;
        match kind {
            K::Database
            | K::Table
            | K::Column
            | K::Index
            | K::ForeignKey
            | K::View
            | K::Function
            | K::Procedure
            | K::Trigger
            | K::Event
            | K::User
            | K::Grant => true,
            K::Role => {
                if self.mariadb {
                    v.at_least_patch(10, 0, 5)
                } else {
                    v.at_least(8, 0)
                }
            }
            K::Check => {
                if self.mariadb {
                    v.at_least_patch(10, 2, 1)
                } else {
                    v.at_least_patch(8, 0, 16)
                }
            }
            K::Sequence => self.mariadb && v.at_least(10, 3),
            K::Schema | K::MatView | K::Type | K::Extension | K::Key => false,
        }
    }
}

// ---- helpers shared by the fetchers ----------------------------------------------------------

/// `true` when the connection talks to MariaDB.
fn is_mariadb(c: &dyn Connection) -> bool {
    c.engine() == Engine::MariaDb
}

/// `true` for the server's own databases.
fn is_system_database(name: &str) -> bool {
    SYSTEM_DATABASES
        .iter()
        .any(|s| s.eq_ignore_ascii_case(name))
}

/// The database `r` lives in (a `Database` ref's own name, else its catalog), or the session's
/// current database when the ref has none.
async fn database_of(
    c: &dyn Connection,
    r: &ObjectRef,
    cancel: &CancellationToken,
) -> Result<String> {
    if let Some(db) = r.database_name().filter(|d| !d.is_empty()) {
        return Ok(db.to_owned());
    }
    query(c, "SELECT DATABASE()", &[], cancel)
        .await?
        .first()
        .and_then(|row| row.non_empty(0))
        .ok_or_else(|| {
            Error::invalid_input(format!(
                "{r} names no database and the session has no default database"
            ))
        })
}

/// `r` with its database made explicit, so refs built from it carry a catalog.
fn scoped(r: &ObjectRef, db: &str) -> ObjectRef {
    if r.kind == ObjectKind::Database {
        ObjectRef::database(db)
    } else {
        r.clone().in_catalog(Some(db))
    }
}

/// The owning table's name of a table child.
fn owner_name(obj: &ObjectRef) -> Result<&str> {
    obj.parent
        .as_deref()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| Error::invalid_input(format!("{obj} names no owning table")))
}

/// A listing sorted by name in byte order, whatever the server's collation (columns keep their
/// ordinal order and do not go through this).
fn by_name(mut list: Vec<ObjectSummary>) -> Vec<ObjectSummary> {
    list.sort_by(|a, b| a.obj.name.cmp(&b.obj.name));
    list
}

/// `Error::NotFound` for `obj`.
fn not_found(obj: &ObjectRef) -> Error {
    Error::not_found(format!("{} {obj} does not exist", obj.kind.display_name()))
}

/// Checks that `obj` has the kind this fetcher reads.
fn expect_kind(obj: &ObjectRef, kind: ObjectKind) -> Result<()> {
    if obj.kind == kind {
        Ok(())
    } else {
        Err(Error::invalid_input(format!(
            "{obj} is not a {}",
            kind.display_name()
        )))
    }
}

#[cfg(test)]
mod tests;
