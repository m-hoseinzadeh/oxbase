//! Moving names inside engine-native SQL text between containers (`docs/m8-contracts.md`
//! §3.2): the part of relocation `oxbase_types::relocate_def` leaves alone. Owner of the body:
//! the **structure-sync** stream (used by structure sync and by restore into another
//! container).
//!
//! Rules for [`retarget_sql`]:
//!
//! - Tokens come from `oxbase_sql::tokenize` with the engine's `SqlDialect`, so names inside
//!   strings and comments are never touched.
//! - An identifier (quoted or not) that names the source schema (PostgreSQL, SQL Server) or the
//!   source database (MySQL, MariaDB; also a SQL Server three-part name's database part) and is
//!   **followed by `.`** is replaced by the target's name, quoted the way the source spelled it
//!   (a quoted identifier stays quoted, with the engine's quoting; an unquoted one stays
//!   unquoted when the target name needs no quotes, else it is quoted).
//! - The comparison follows the engine's identifier rules: unquoted PostgreSQL identifiers fold
//!   to lower case, MySQL / MariaDB / SQL Server compare case-insensitively, quoted identifiers
//!   exactly.
//! - Nothing else changes (no reformatting): text without the source name comes back equal.
//! - SQLite: no schemas to move (`main` stays `main`); the text is returned unchanged.

use oxbase_types::{Engine, Location};

/// `text` (engine-native SQL of `engine`) with every qualified reference into `from` moved to
/// `to` (module docs).
pub fn retarget_sql(engine: Engine, text: &str, from: &Location, to: &Location) -> String {
    let _ = (engine, text, from, to);
    unimplemented!("m8: structure-sync")
}
