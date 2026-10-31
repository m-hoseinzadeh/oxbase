//! What the table designer may offer on PostgreSQL (M3), by server version.

use oxbase_dialect::{DesignerCapabilities, Support};
use oxbase_types::{FkAction, ServerVersion};

use super::sql::at_least;

/// The designer capabilities of a PostgreSQL server of version `v` (unknown versions count as
/// current). Everything is native and transactional; columns cannot be reordered.
pub(crate) fn designer(v: &ServerVersion) -> DesignerCapabilities {
    let native = Support::Native;
    let mut notes = vec![
        "PostgreSQL runs the whole change in one transaction: if any statement fails, nothing \
         is changed."
            .to_owned(),
        "PostgreSQL cannot reorder columns: new columns are added at the end.".to_owned(),
        "Changing a column's type rewrites the table and may fail on existing values; it holds \
         an exclusive lock on the table while it runs."
            .to_owned(),
    ];
    if !at_least(v, 17) {
        notes.push(
            "Changing the expression of a generated column needs PostgreSQL 17 or newer."
                .to_owned(),
        );
    }
    DesignerCapabilities {
        transactional_ddl: true,
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
        reorder_columns: Support::Unsupported,
        auto_increment: true,
        identity: at_least(v, 10),
        generated_stored: at_least(v, 12),
        generated_virtual: at_least(v, 18),
        change_primary_key: native,
        add_index: native,
        drop_index: native,
        unique_constraints: true,
        partial_indexes: true,
        expression_indexes: true,
        descending_indexes: true,
        index_prefix_lengths: false,
        index_methods: Vec::from(
            ["btree", "hash", "gist", "spgist", "gin", "brin"].map(str::to_owned),
        ),
        add_foreign_key: native,
        drop_foreign_key: native,
        fk_actions: vec![
            FkAction::NoAction,
            FkAction::Restrict,
            FkAction::Cascade,
            FkAction::SetNull,
            FkAction::SetDefault,
        ],
        deferrable_constraints: true,
        add_check: native,
        drop_check: native,
        checks_enforced: true,
        max_identifier_length: Some(63),
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_the_server_version() {
        let v18 = designer(&ServerVersion::new(18, 6, 0));
        assert!(v18.transactional_ddl && v18.identity && v18.generated_stored);
        assert!(v18.generated_virtual);
        assert_eq!(v18.reorder_columns, Support::Unsupported);
        assert_eq!(v18.alter_column_type, Support::Native);
        assert_eq!(v18.max_identifier_length, Some(63));
        assert_eq!(v18.fk_actions.len(), 5);
        assert!(!v18.index_prefix_lengths);
        assert_eq!(v18.notes.len(), 3);

        let v11 = designer(&ServerVersion::new(11, 22, 0));
        assert!(v11.identity);
        assert!(!v11.generated_stored && !v11.generated_virtual);
        assert_eq!(v11.notes.len(), 4);

        let v12 = designer(&ServerVersion::new(12, 0, 0));
        assert!(v12.generated_stored && !v12.generated_virtual);

        let unknown = designer(&ServerVersion::new(0, 0, 0));
        assert!(
            unknown.generated_virtual,
            "an unknown version counts as current"
        );

        let json = serde_json::to_value(&v18).unwrap();
        assert_eq!(json["addColumn"], "native");
        assert_eq!(json["reorderColumns"], "unsupported");
    }
}
