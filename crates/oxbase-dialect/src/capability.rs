//! What the table designer may offer on an engine and server version (M3):
//! [`DesignerCapabilities`], returned by [`Capabilities::designer`](crate::Capabilities::designer)
//! and sent to the UI by `capabilities_get` (camelCase JSON).
//!
//! The UI greys out what is [`Support::Unsupported`], marks [`Support::Rebuild`] operations as
//! "rebuilds the table" (destructive steps in the preview), and shows [`DesignerCapabilities::notes`]
//! (for example "MySQL commits each DDL statement on its own"). The DDL generator is the
//! authority: an operation the capabilities call unsupported fails in the generator with
//! `Unsupported`, never silently.

use oxbase_types::FkAction;
use serde::{Deserialize, Serialize};

/// How an engine performs one kind of change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Support {
    /// In place, with the engine's own statement (`ALTER TABLE ...`).
    Native,
    /// Only by rebuilding the table (SQLite's documented procedure: create the new table, copy
    /// the rows, drop the old one, rename, re-create indexes and triggers). The preview marks it
    /// destructive.
    Rebuild,
    /// Not offered.
    #[default]
    Unsupported,
}

impl Support {
    /// `true` unless [`Support::Unsupported`].
    pub const fn is_supported(self) -> bool {
        !matches!(self, Self::Unsupported)
    }
}

/// What the table designer may offer. `Default` is a read-only designer (nothing supported);
/// engines fill it per server version in `Capabilities::designer`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DesignerCapabilities {
    /// DDL runs inside a transaction and rolls back (PostgreSQL, SQLite). `false` on MySQL and
    /// MariaDB, where every DDL statement commits on its own: a failing script leaves the
    /// earlier statements applied, and the preview says so.
    pub transactional_ddl: bool,
    /// `CREATE TABLE` from the designer.
    pub create_table: bool,
    pub drop_table: bool,
    pub rename_table: Support,
    pub table_comment: Support,

    pub add_column: Support,
    pub drop_column: Support,
    pub rename_column: Support,
    /// Change a column's type (PostgreSQL `ALTER COLUMN TYPE ... USING`, MySQL `MODIFY`).
    pub alter_column_type: Support,
    pub alter_column_nullability: Support,
    pub alter_column_default: Support,
    pub column_comment: Support,
    /// Per-column collation.
    pub column_collation: Support,
    /// Put a column at another position (MySQL `AFTER` / `FIRST`; elsewhere a rebuild or not
    /// at all).
    pub reorder_columns: Support,
    /// `AUTO_INCREMENT` / `serial`-style generated keys.
    pub auto_increment: bool,
    /// SQL-standard identity columns (`GENERATED ... AS IDENTITY`).
    pub identity: bool,
    /// Stored generated (computed) columns.
    pub generated_stored: bool,
    /// Virtual generated columns.
    pub generated_virtual: bool,

    /// Add, drop or change the primary key.
    pub change_primary_key: Support,

    pub add_index: Support,
    pub drop_index: Support,
    pub unique_constraints: bool,
    /// `WHERE` predicates on indexes.
    pub partial_indexes: bool,
    /// Expressions as index keys.
    pub expression_indexes: bool,
    pub descending_indexes: bool,
    /// Prefix lengths on index keys (MySQL `col(10)`).
    pub index_prefix_lengths: bool,
    /// Index access methods the designer offers (`btree`, `hash`, `gin`, ...); empty: only the
    /// engine default.
    pub index_methods: Vec<String>,

    pub add_foreign_key: Support,
    pub drop_foreign_key: Support,
    /// Referential actions the engine accepts.
    pub fk_actions: Vec<FkAction>,
    /// `DEFERRABLE` constraints.
    pub deferrable_constraints: bool,

    pub add_check: Support,
    pub drop_check: Support,
    /// Checks are enforced. `false` on MySQL before 8.0.16, which parses `CHECK` and ignores
    /// it; the designer then warns.
    pub checks_enforced: bool,

    /// Longest identifier in bytes (PostgreSQL 63, MySQL/MariaDB 64); `None` when unlimited.
    pub max_identifier_length: Option<u32>,
    /// Human notes the designer shows with the preview (transaction behaviour, rebuilds,
    /// unenforced checks, ...).
    pub notes: Vec<String>,
}

impl DesignerCapabilities {
    /// Nothing supported: the designer only shows the table (the `Default`).
    pub fn read_only() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn default_is_read_only_and_serde_is_camel_case() {
        let caps = DesignerCapabilities::read_only();
        assert!(!caps.add_column.is_supported());
        assert!(Support::Rebuild.is_supported());
        let mut pg = DesignerCapabilities {
            transactional_ddl: true,
            add_column: Support::Native,
            drop_column: Support::Rebuild,
            max_identifier_length: Some(63),
            fk_actions: vec![FkAction::Cascade, FkAction::SetNull],
            ..DesignerCapabilities::default()
        };
        pg.notes.push("note".into());
        let json = serde_json::to_value(&pg).unwrap();
        assert_eq!(json["transactionalDdl"], true);
        assert_eq!(json["addColumn"], "native");
        assert_eq!(json["dropColumn"], "rebuild");
        assert_eq!(json["renameColumn"], "unsupported");
        assert_eq!(json["maxIdentifierLength"], 63);
        assert_eq!(
            json["fkActions"],
            serde_json::json!(["cascade", "set_null"])
        );
        assert_eq!(
            serde_json::from_value::<DesignerCapabilities>(json).unwrap(),
            pg
        );
        // Missing fields default (older or newer peers).
        let partial: DesignerCapabilities =
            serde_json::from_str(r#"{"addIndex":"native"}"#).unwrap();
        assert_eq!(partial.add_index, Support::Native);
        assert!(!partial.transactional_ddl);
    }
}
