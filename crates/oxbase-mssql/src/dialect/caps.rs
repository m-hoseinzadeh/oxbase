//! What SQL Server supports by server version: object kinds and properties (Design, final for
//! M4) and the table designer's capabilities ([`designer`], **ddl** stream). Contract:
//! `docs/m4-contracts.md` §4.4.

use oxbase_dialect::{Capabilities, DesignerCapabilities, Support};
use oxbase_types::{FkAction, ObjectKind, PropId, ServerVersion};

/// SQL Server's [`Capabilities`]. Unknown versions (`0.0.0`) count as current.
#[derive(Debug, Clone, Copy, Default)]
pub struct MsSqlCapabilities;

impl Capabilities for MsSqlCapabilities {
    fn supports(&self, kind: ObjectKind, prop: PropId, v: &ServerVersion) -> bool {
        if !self.supports_kind(kind, v) {
            return false;
        }
        !matches!(
            prop.as_str(),
            // PostgreSQL / MySQL / MariaDB / SQLite only.
            "charset"
                | "storage_engine"
                | "row_format"
                | "without_rowid"
                | "strict"
                | "unlogged"
                | "tablespace"
                | "partition_by"
                | "index_method"
                | "deferrable"
                | "initially_deferred"
                | "deterministic"
                | "volatility"
                | "strict_input"
                | "sql_data_access"
                | "definer"
                | "view_algorithm"
                | "populated"
                | "temporary"
                | "encoding"
                | "file"
        )
    }

    fn supports_kind(&self, kind: ObjectKind, v: &ServerVersion) -> bool {
        let _ = v;
        !matches!(
            kind,
            ObjectKind::MatView | ObjectKind::Event | ObjectKind::Extension | ObjectKind::Key
        )
    }

    fn designer(&self, v: &ServerVersion) -> DesignerCapabilities {
        designer(v)
    }
}

/// Longest SQL Server identifier, in characters (`sysname` is `nvarchar(128)`).
pub(crate) const MAX_IDENTIFIER_LENGTH: u32 = 128;

/// The notes the designer shows with every SQL Server preview, in this order.
pub(crate) const NOTES: [&str; 5] = [
    "SQL Server runs the whole change in one transaction: if any statement fails, nothing is \
     changed.",
    "SQL Server cannot reorder columns: new columns are added at the end.",
    "IDENTITY cannot be added to or removed from an existing column.",
    "Changing a column's type drops and recreates the default, indexes, keys and checks that use \
     it.",
    "Renames use sp_rename: views, procedures and scripts that use the old name are not updated.",
];

/// The designer capabilities of a SQL Server of version `v` (ddl stream). Expected values:
/// `docs/m4-contracts.md` §4.4. Every supported version (2017+) has the same designer, and an
/// unknown version (`0.0.0`) counts as current.
///
/// Clustering is not a designer capability: it is the `clustered` property of indexes and of
/// the primary key (`Capabilities::supports(Index, CLUSTERED)`), so `index_methods` is empty.
pub(crate) fn designer(v: &ServerVersion) -> DesignerCapabilities {
    let _ = v;
    let native = Support::Native;
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
        auto_increment: false,
        identity: true,
        generated_stored: true,
        generated_virtual: true,
        change_primary_key: native,
        add_index: native,
        drop_index: native,
        unique_constraints: true,
        partial_indexes: true,
        expression_indexes: false,
        descending_indexes: true,
        index_prefix_lengths: false,
        index_methods: Vec::new(),
        add_foreign_key: native,
        drop_foreign_key: native,
        fk_actions: vec![
            FkAction::NoAction,
            FkAction::Cascade,
            FkAction::SetNull,
            FkAction::SetDefault,
        ],
        deferrable_constraints: false,
        add_check: native,
        drop_check: native,
        checks_enforced: true,
        max_identifier_length: Some(MAX_IDENTIFIER_LENGTH),
        notes: NOTES.map(str::to_owned).to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::props;

    #[test]
    fn kinds_and_properties() {
        let caps = MsSqlCapabilities;
        let v = ServerVersion::new(17, 0, 0);
        assert!(caps.supports_kind(ObjectKind::Sequence, &v));
        assert!(caps.supports_kind(ObjectKind::Type, &v));
        assert!(!caps.supports_kind(ObjectKind::MatView, &v));
        assert!(!caps.supports_kind(ObjectKind::Key, &v));
        assert!(caps.supports(ObjectKind::Index, props::CLUSTERED, &v));
        assert!(caps.supports(ObjectKind::Index, props::INCLUDE_COLUMNS, &v));
        assert!(caps.supports(ObjectKind::Column, props::GENERATED, &v));
        assert!(caps.supports(ObjectKind::Column, crate::extras::ROW_VERSION, &v));
        assert!(!caps.supports(ObjectKind::Table, props::STORAGE_ENGINE, &v));
        assert!(!caps.supports(ObjectKind::Index, props::INDEX_METHOD, &v));
        assert!(!caps.supports(ObjectKind::MatView, props::COMMENT, &v));
    }

    #[test]
    fn designer_values_match_the_contract() {
        let caps = MsSqlCapabilities;
        let d = caps.designer(&ServerVersion::new(17, 0, 4005));
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["transactionalDdl"], true);
        assert_eq!(json["createTable"], true);
        assert_eq!(json["dropTable"], true);
        for key in [
            "renameTable",
            "tableComment",
            "addColumn",
            "dropColumn",
            "renameColumn",
            "alterColumnType",
            "alterColumnNullability",
            "alterColumnDefault",
            "columnComment",
            "columnCollation",
            "changePrimaryKey",
            "addIndex",
            "dropIndex",
            "addForeignKey",
            "dropForeignKey",
            "addCheck",
            "dropCheck",
        ] {
            assert_eq!(json[key], "native", "{key}");
        }
        assert_eq!(json["reorderColumns"], "unsupported");
        assert_eq!(json["autoIncrement"], false);
        assert_eq!(json["identity"], true);
        assert_eq!(json["generatedStored"], true);
        assert_eq!(json["generatedVirtual"], true);
        assert_eq!(json["uniqueConstraints"], true);
        assert_eq!(json["partialIndexes"], true);
        assert_eq!(json["descendingIndexes"], true);
        assert_eq!(json["expressionIndexes"], false);
        assert_eq!(json["indexPrefixLengths"], false);
        assert_eq!(json["deferrableConstraints"], false);
        assert_eq!(json["indexMethods"], serde_json::json!([]));
        assert_eq!(
            json["fkActions"],
            serde_json::json!(["no_action", "cascade", "set_null", "set_default"])
        );
        assert_eq!(json["checksEnforced"], true);
        assert_eq!(json["maxIdentifierLength"], 128);
        assert_eq!(d.notes.len(), 5);
        assert!(d.notes[0].starts_with("SQL Server runs the whole change in one transaction"));
        assert!(d.notes[1].contains("cannot reorder columns"));
        assert!(d.notes[2].starts_with("IDENTITY cannot be added"));
        assert!(d.notes[3].starts_with("Changing a column's type drops and recreates"));
        assert!(d.notes[4].starts_with("Renames use sp_rename"));
        // The same designer for every supported version, and for an unknown one.
        assert_eq!(caps.designer(&ServerVersion::new(14, 0, 0)), d);
        assert_eq!(caps.designer(&ServerVersion::new(0, 0, 0)), d);
        assert!(caps.supports(
            ObjectKind::Index,
            props::CLUSTERED,
            &ServerVersion::new(14, 0, 0)
        ));
    }
}
