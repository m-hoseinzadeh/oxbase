//! SQL dump writer (write stream): `CREATE TABLE` from the target engine's `DdlGenerator` and
//! `INSERT` statements whose literals the target engine's `Quoting` escapes.
//!
//! # Contract
//!
//! - **Target.** [`SqlDumpOptions::target`] picks the engine; its cells come from
//!   [`crate::WriteContext::dialects`] (`Unsupported` when the registry has no `DdlGenerator`
//!   for `(target, Table)` or no `Quoting`). The version is
//!   [`SqlDumpOptions::target_version`] parsed with `ServerVersion::parse`, else
//!   [`default_version`].
//! - **Table definition.** [`crate::WriteContext::table`] when the export is a table: its
//!   columns, primary key, indexes, checks and comments. When the target engine differs from
//!   [`crate::WriteContext::source_engine`], every column's `native_type`, `default`,
//!   `collation`, generated expression and `extras` are cleared (they are the source engine's
//!   SQL) and foreign keys are dropped (they point at tables the dump does not contain); the
//!   dump starts with a `--` comment saying so. Without a table (a query or result export) the
//!   definition is built from the batches' schema with [`crate::logical_type`]: nullable
//!   columns, no keys. The table name is [`SqlDumpOptions::table`], else the source table's,
//!   else `export`.
//! - **Statements.** A header comment (`-- Oxbase SQL dump`, target engine and version, no
//!   connection details); `DROP TABLE` (`include_drop`) via `DdlGenerator::drop_object`; the
//!   `CREATE` script (`include_create`) via `DdlGenerator::create` with `DdlContext::new(version)`
//!   and `Script::to_sql(";")`; then `INSERT INTO <qualified> (<quoted columns>) VALUES
//!   (...), (...);` with [`SqlDumpOptions::rows_per_insert`] rows each (SQL Server: at most
//!   1000), every value rendered with `Quoting::literal` of
//!   [`oxbase_driver::cell_value`] (strings and binary escaped by the target dialect: PostgreSQL
//!   `E'...'` / `'\x..'::bytea`, MySQL backslashes, SQL Server `N'...'` / `0x..`). Statements
//!   end with `;` (no `GO` lines). SQL Server: `SET IDENTITY_INSERT <table> ON` / `OFF` around
//!   the inserts when the table has an identity column whose values are written.
//! - Line ends `\n`, UTF-8 without BOM. Progress in bytes written.

use arrow_schema::SchemaRef;
use oxbase_types::{Engine, Result, ServerVersion};

use crate::{SinkFile, SinkWriter, SqlDumpOptions, WriteContext};

/// The server version a dump targets when [`SqlDumpOptions::target_version`] is not set: the
/// fixture versions of M4 (PostgreSQL 18, MySQL 8.4, MariaDB 11.8, SQLite 3.45, SQL Server
/// 2025 = 17). `None` for engines without SQL dumps.
pub fn default_version(engine: Engine) -> Option<ServerVersion> {
    match engine {
        Engine::Postgres => Some(ServerVersion::new(18, 0, 0)),
        Engine::MySql => Some(ServerVersion::new(8, 4, 0)),
        Engine::MariaDb => Some(ServerVersion::new(11, 8, 0)),
        Engine::Sqlite => Some(ServerVersion::new(3, 45, 0)),
        Engine::MsSql => Some(ServerVersion::new(17, 0, 0)),
        Engine::MongoDb | Engine::Redis => None,
    }
}

/// The SQL dump writer over `file` (see [`crate::create_writer`]).
pub fn writer(
    file: SinkFile,
    schema: SchemaRef,
    options: &SqlDumpOptions,
    context: &WriteContext,
) -> Result<Box<dyn SinkWriter>> {
    let _ = (file, schema, options, context);
    unimplemented!("m5: write")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_versions_exist_for_sql_engines() {
        for engine in [
            Engine::Postgres,
            Engine::MySql,
            Engine::MariaDb,
            Engine::Sqlite,
            Engine::MsSql,
        ] {
            assert!(default_version(engine).is_some(), "{engine}");
        }
        assert!(default_version(Engine::Redis).is_none());
    }
}
