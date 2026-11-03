//! Data transfer (M8): copy tables, views and query results from one connection to another,
//! across engines, with a per-column type mapping report. Contract: `docs/m8-contracts.md` §4.
//!
//! Module map:
//!
//! - [`spec`]: what to copy and how ([`TransferSpec`]: source objects, target container,
//!   [`TransferOptions`], column overrides).
//! - [`mapping`]: the pure mapping of columns and tables ([`mapping::map_column`],
//!   [`mapping::target_table`], the per-pair report [`mapping::pair_report`]).
//! - [`cells`]: the per-engine [`oxbase_dialect::TypeMapper`] cells, registered by
//!   [`register`].
//! - [`convert`]: source Arrow pages → load batches ([`convert::BatchConverter`]).
//! - [`pipeline`]: the plan / dry run ([`pipeline::plan`]) and the per-table copy loop
//!   ([`pipeline::copy_table`]) on two connections.
//!
//! Sessions, the event stream, cancellation and "closing a connection cancels its transfers"
//! are `oxbase_services::transfer`. This crate never opens a connection itself.
//!
//! MongoDB and Redis are **not** transfer endpoints in M8 (`docs/m8-contracts.md` §4.6): the
//! plan answers `Unsupported` for them.

pub mod cells;
pub mod convert;
pub mod mapping;
pub mod pipeline;
pub mod spec;

use std::sync::Arc;

use oxbase_dialect::DialectRegistry;
use oxbase_types::Engine;

pub use mapping::{ColumnPlan, MappingContext, PairReport, PairRow};
pub use pipeline::{
    TablePlan, TableProgress, TableReport, TableStatus, TransferPlan, TransferReport,
};
pub use spec::{
    ColumnOverride, ErrorMode, FkChecks, IdentityMode, SourceObject, TableMode, TransactionScope,
    TransferOptions, TransferSpec,
};

/// Registers the type mapper of each SQL engine (PostgreSQL, MySQL, MariaDB, SQLite, SQL
/// Server) in `reg`. Called by `oxbase_services::default_dialects`.
pub fn register(reg: &mut DialectRegistry) {
    reg.register_type_mapper(Engine::Postgres, Arc::new(cells::postgres::PgTypeMapper))
        .register_type_mapper(
            Engine::MySql,
            Arc::new(cells::mysql::MySqlTypeMapper { mariadb: false }),
        )
        .register_type_mapper(
            Engine::MariaDb,
            Arc::new(cells::mysql::MySqlTypeMapper { mariadb: true }),
        )
        .register_type_mapper(Engine::Sqlite, Arc::new(cells::sqlite::SqliteTypeMapper))
        .register_type_mapper(Engine::MsSql, Arc::new(cells::mssql::MsSqlTypeMapper));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_a_mapper_for_every_sql_engine() {
        let mut reg = DialectRegistry::new();
        register(&mut reg);
        for engine in oxbase_dialect::M8_ENGINES {
            assert!(reg.has_type_mapper(engine), "{engine}");
        }
        assert!(!reg.has_type_mapper(Engine::MongoDb));
    }
}
