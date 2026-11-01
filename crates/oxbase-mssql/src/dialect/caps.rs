//! What SQL Server supports by server version: object kinds and properties (Design, final for
//! M4) and the table designer's capabilities ([`designer`], **ddl** stream). Contract:
//! `docs/m4-contracts.md` §4.4.

use oxbase_dialect::{Capabilities, DesignerCapabilities};
use oxbase_types::{ObjectKind, PropId, ServerVersion};

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

/// The designer capabilities of a SQL Server of version `v` (ddl stream). Expected values:
/// `docs/m4-contracts.md` §4.4.
pub(crate) fn designer(v: &ServerVersion) -> DesignerCapabilities {
    let _ = v;
    unimplemented!("m4: ddl (designer capabilities)")
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
}
