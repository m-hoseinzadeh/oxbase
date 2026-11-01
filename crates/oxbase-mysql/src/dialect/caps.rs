//! What the table designer and the generators may use on a MySQL or MariaDB server version (M3):
//! [`Features`] (shared by the DDL generator and the DML writer) and the
//! [`DesignerCapabilities`] built from them.

use oxbase_dialect::{DesignerCapabilities, Support};
use oxbase_types::{FkAction, ServerVersion};

/// Longest identifier, in characters, on both engines.
pub(crate) const MAX_IDENTIFIER_LENGTH: u32 = 64;

/// Version-dependent syntax of one server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Features {
    pub mariadb: bool,
    /// `ALTER TABLE ... RENAME COLUMN a TO b` (MySQL 8.0, MariaDB 10.5.2); else `CHANGE`.
    pub rename_column: bool,
    /// `ALTER TABLE ... RENAME INDEX a TO b` (MySQL 5.7, MariaDB 10.5.2); else drop + add.
    pub rename_index: bool,
    /// Check constraints are stored and enforced (MySQL 8.0.16, MariaDB 10.2.1).
    pub checks: bool,
    /// `DESC` index keys are stored (MySQL 8.0, MariaDB 10.8).
    pub descending_indexes: bool,
    /// Functional key parts `((expr))` (MySQL 8.0.13).
    pub expression_indexes: bool,
    /// `INVISIBLE` columns (MySQL 8.0.23, MariaDB 10.3.3).
    pub invisible_columns: bool,
    /// Invisible indexes: MySQL 8.0 `INVISIBLE`, MariaDB 10.6 `IGNORED`.
    pub invisible_indexes: bool,
    /// `INSERT ... RETURNING` (MariaDB 10.5).
    pub insert_returning: bool,
}

impl Features {
    pub(crate) fn new(mariadb: bool, v: &ServerVersion) -> Self {
        if mariadb {
            Self {
                mariadb,
                rename_column: v.at_least_patch(10, 5, 2),
                rename_index: v.at_least_patch(10, 5, 2),
                checks: v.at_least_patch(10, 2, 1),
                descending_indexes: v.at_least(10, 8),
                expression_indexes: false,
                invisible_columns: v.at_least_patch(10, 3, 3),
                invisible_indexes: v.at_least(10, 6),
                insert_returning: v.at_least(10, 5),
            }
        } else {
            Self {
                mariadb,
                rename_column: v.at_least(8, 0),
                rename_index: v.at_least(5, 7),
                checks: v.at_least_patch(8, 0, 16),
                descending_indexes: v.at_least(8, 0),
                expression_indexes: v.at_least_patch(8, 0, 13),
                invisible_columns: v.at_least_patch(8, 0, 23),
                invisible_indexes: v.at_least(8, 0),
                insert_returning: false,
            }
        }
    }

    /// The engine's name for messages.
    pub(crate) fn engine_name(self) -> &'static str {
        if self.mariadb { "MariaDB" } else { "MySQL" }
    }
}

/// The designer capabilities of MySQL (`mariadb = false`) or MariaDB at version `v`.
pub(crate) fn designer(mariadb: bool, v: &ServerVersion) -> DesignerCapabilities {
    let f = Features::new(mariadb, v);
    let native = Support::Native;
    let mut notes = vec![format!(
        "{} commits each DDL statement on its own: if a statement fails, the statements before \
         it stay applied.",
        f.engine_name()
    )];
    if !f.checks {
        notes.push(format!(
            "{} {v} parses CHECK constraints but neither stores nor enforces them.",
            f.engine_name()
        ));
    }
    notes.push(
        "InnoDB has no SET DEFAULT foreign-key action, and every foreign key needs an index on \
         its columns (InnoDB creates one named after the constraint when there is none)."
            .to_owned(),
    );
    notes.push(if mariadb {
        "Foreign key names are unique per database, check names per table; a check without a \
         name is named by the server. The primary key is always named PRIMARY."
            .to_owned()
    } else {
        "Foreign key and check names are unique per database; a check without a name is named \
         <table>_chk_<n> by the server. The primary key is always named PRIMARY."
            .to_owned()
    });
    DesignerCapabilities {
        transactional_ddl: false,
        create_table: true,
        drop_table: true,
        rename_table: native,
        table_comment: native,
        add_column: native,
        drop_column: native,
        rename_column: native,
        alter_column_type: native,
        alter_column_nullability: native,
        alter_column_default: native,
        column_comment: native,
        column_collation: native,
        reorder_columns: native,
        auto_increment: true,
        identity: false,
        generated_stored: true,
        generated_virtual: true,
        change_primary_key: native,
        add_index: native,
        drop_index: native,
        unique_constraints: true,
        partial_indexes: false,
        expression_indexes: f.expression_indexes,
        descending_indexes: f.descending_indexes,
        index_prefix_lengths: true,
        index_methods: ["BTREE", "HASH", "FULLTEXT", "SPATIAL"]
            .map(str::to_owned)
            .to_vec(),
        add_foreign_key: native,
        drop_foreign_key: native,
        fk_actions: vec![
            FkAction::NoAction,
            FkAction::Restrict,
            FkAction::Cascade,
            FkAction::SetNull,
        ],
        deferrable_constraints: false,
        add_check: native,
        drop_check: if f.checks {
            native
        } else {
            Support::Unsupported
        },
        checks_enforced: f.checks,
        max_identifier_length: Some(MAX_IDENTIFIER_LENGTH),
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> ServerVersion {
        ServerVersion::parse(text).unwrap()
    }

    #[test]
    fn features_by_version() {
        let mysql84 = Features::new(false, &v("8.4.6"));
        assert!(mysql84.rename_column && mysql84.checks && mysql84.expression_indexes);
        assert!(!mysql84.insert_returning);
        let mysql57 = Features::new(false, &v("5.7.44"));
        assert!(!mysql57.rename_column && mysql57.rename_index && !mysql57.checks);
        let maria118 = Features::new(true, &v("11.8.9-MariaDB-ubu2404"));
        assert!(maria118.insert_returning && maria118.descending_indexes && maria118.checks);
        assert!(!maria118.expression_indexes);
        let maria104 = Features::new(true, &v("5.5.5-10.4.34-MariaDB"));
        assert!(!maria104.rename_column && !maria104.insert_returning);
        assert!(maria104.checks && !maria104.invisible_indexes);
    }

    #[test]
    fn designer_capabilities() {
        let caps = designer(false, &v("8.4.6"));
        assert!(!caps.transactional_ddl);
        assert_eq!(caps.rename_column, Support::Native);
        assert!(caps.checks_enforced);
        assert_eq!(caps.max_identifier_length, Some(64));
        assert!(!caps.fk_actions.contains(&FkAction::SetDefault));
        assert!(caps.notes[0].contains("commits each DDL statement"));
        let json = serde_json::to_value(&caps).unwrap();
        assert_eq!(json["transactionalDdl"], false);
        assert_eq!(json["dropCheck"], "native");

        let old = designer(false, &v("8.0.15"));
        assert!(!old.checks_enforced);
        assert_eq!(old.drop_check, Support::Unsupported);
        assert!(old.notes.iter().any(|n| n.contains("CHECK")));
        assert!(old.expression_indexes);
        assert!(!designer(false, &v("5.7.44")).expression_indexes);

        let maria = designer(true, &v("11.8.9-MariaDB"));
        assert!(maria.checks_enforced && maria.descending_indexes);
        assert!(!maria.expression_indexes && !maria.identity);
        assert!(maria.notes[0].starts_with("MariaDB"));
        assert!(
            maria
                .notes
                .iter()
                .any(|n| n.contains("check names per table"))
        );
        assert!(caps.notes.iter().any(|n| n.contains("unique per database")));
    }
}
