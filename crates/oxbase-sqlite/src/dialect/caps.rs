//! What the table designer may offer on SQLite (M3), by library version: the answer of
//! `SqliteCapabilities::designer`.
//!
//! SQLite alters a table in place only for `RENAME TO`, `RENAME COLUMN` (3.25+), `ADD COLUMN` and
//! `DROP COLUMN` (3.35+, plain columns only) and for `CREATE` / `DROP INDEX`. Every other change
//! (column type, nullability, default, collation, order, primary key, foreign keys, checks, and
//! the cases `ADD` / `DROP COLUMN` refuse) is a [`Support::Rebuild`]: the documented table
//! rebuild, inside the script's transaction (see `ddl`). There are no comments, no identity
//! columns and no index prefix lengths.
//!
//! `UNIQUE` constraints that already exist are kept (re-declared by a rebuild), but the designer
//! cannot add one: SQLite names their indexes itself (`sqlite_autoindex_<table>_<n>`), so a
//! constraint name given by the designer would not survive. Unique indexes do the same job.

use oxbase_dialect::{DesignerCapabilities, Support};
use oxbase_types::{FkAction, ServerVersion};

/// The designer capabilities of SQLite `v`.
pub fn designer_capabilities(v: &ServerVersion) -> DesignerCapabilities {
    let native_from = |major: u32, minor: u32| {
        if v.at_least(major, minor) {
            Support::Native
        } else {
            Support::Rebuild
        }
    };
    let generated = v.at_least(3, 31);
    let mut notes = vec![
        "SQLite changes column types, nullability, defaults, collations, column order, the \
         primary key, foreign keys and checks by rebuilding the table: a new table is created, \
         the rows are copied, the old table is dropped and the new one renamed, then its \
         indexes and triggers are re-created. Views on the table are kept."
            .to_owned(),
        "The whole script runs in one transaction; foreign-key enforcement is switched off \
         during a rebuild and back on afterwards, and the rebuilt table's foreign keys are \
         checked before the commit."
            .to_owned(),
        "SQLite has no table or column comments.".to_owned(),
    ];
    if !v.at_least(3, 35) {
        notes.push("DROP COLUMN needs SQLite 3.35; this version rebuilds the table.".to_owned());
    }
    DesignerCapabilities {
        transactional_ddl: true,
        create_table: true,
        drop_table: true,
        rename_table: Support::Native,
        table_comment: Support::Unsupported,
        add_column: Support::Native,
        drop_column: native_from(3, 35),
        rename_column: native_from(3, 25),
        alter_column_type: Support::Rebuild,
        alter_column_nullability: Support::Rebuild,
        alter_column_default: Support::Rebuild,
        column_comment: Support::Unsupported,
        column_collation: Support::Rebuild,
        reorder_columns: Support::Rebuild,
        auto_increment: true,
        identity: false,
        generated_stored: generated,
        generated_virtual: generated,
        change_primary_key: Support::Rebuild,
        add_index: Support::Native,
        drop_index: Support::Native,
        unique_constraints: false,
        partial_indexes: v.at_least(3, 8),
        expression_indexes: v.at_least(3, 9),
        descending_indexes: true,
        index_prefix_lengths: false,
        index_methods: Vec::new(),
        add_foreign_key: Support::Rebuild,
        drop_foreign_key: Support::Rebuild,
        fk_actions: vec![
            FkAction::NoAction,
            FkAction::Restrict,
            FkAction::Cascade,
            FkAction::SetNull,
            FkAction::SetDefault,
        ],
        deferrable_constraints: true,
        add_check: Support::Rebuild,
        drop_check: Support::Rebuild,
        checks_enforced: true,
        max_identifier_length: None,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_the_version() {
        let new = designer_capabilities(&"3.51.0".parse().unwrap());
        assert!(new.transactional_ddl && new.create_table && new.drop_table);
        assert_eq!(new.drop_column, Support::Native);
        assert_eq!(new.rename_column, Support::Native);
        assert_eq!(new.alter_column_type, Support::Rebuild);
        assert_eq!(new.add_foreign_key, Support::Rebuild);
        assert_eq!(new.table_comment, Support::Unsupported);
        assert_eq!(new.column_comment, Support::Unsupported);
        assert!(new.generated_stored && !new.identity && !new.unique_constraints);
        assert_eq!(new.fk_actions.len(), 5);
        assert_eq!(new.max_identifier_length, None);
        assert_eq!(new.notes.len(), 3);

        let old = designer_capabilities(&"3.24.0".parse().unwrap());
        assert_eq!(old.drop_column, Support::Rebuild);
        assert_eq!(old.rename_column, Support::Rebuild);
        assert!(!old.generated_stored && !old.generated_virtual);
        assert!(old.notes.iter().any(|n| n.contains("3.35")));

        let mid = designer_capabilities(&"3.31.0".parse().unwrap());
        assert_eq!(mid.rename_column, Support::Native);
        assert_eq!(mid.drop_column, Support::Rebuild);
        assert!(mid.generated_stored);
    }
}
