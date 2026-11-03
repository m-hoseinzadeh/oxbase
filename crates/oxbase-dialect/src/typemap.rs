//! Cross-engine type mapping (M8): the per-engine [`TypeMapper`] role that data transfer uses to
//! decide what type a copied column gets on the target engine, and how much of the source's
//! values that type keeps ([`Fidelity`]). Contract: `docs/m8-contracts.md` §4.1.
//!
//! # The logical type model
//!
//! The engine-neutral model is [`oxbase_types::DataType`] (the same one fetchers, import and the
//! grid use): a column read on the source becomes a logical type ([`TypeMapper::logical`] of the
//! source engine), and the target engine says how it stores that logical type
//! ([`TypeMapper::map`] of the target engine). Rows travel as Arrow load batches in the target
//! column's load schema (`crate::bulk::load_arrow_type`), so the logical type the target column
//! gets ([`TypeMapping::data_type`]) is also what the converter produces.
//!
//! # Fidelity
//!
//! | [`Fidelity`] | Meaning | Example |
//! |---|---|---|
//! | `exact` | every value of the logical type round-trips unchanged | `int64` → MySQL `bigint` |
//! | `widened` | every value is kept, in a wider or more general type (text form included) | PostgreSQL `uuid` → MySQL `char(36)`, `int[]` → MySQL `json` |
//! | `narrowed` | some values are cut, rounded, shifted or refused at load time | `timestamptz` → MySQL `datetime(6)` (offset dropped, UTC instant kept), unconstrained `numeric` → `decimal(65,30)` |
//! | `unsupported` | no target type; the column cannot be copied unless the user skips it or overrides its type | MySQL `geometry` → SQLite |
//!
//! A mapping that is not `exact` always carries a human `reason`, which the transfer wizard
//! shows next to the column's badge.
//!
//! # Rules for implementations (transfer stream)
//!
//! - **Pure.** No I/O: the answer depends only on the logical type and the server version.
//! - **Total.** `map` answers every [`DataType`] variant (`unsupported` when there is no type).
//! - **Native text.** [`TypeMapping::native_type`] is what the target `CREATE TABLE` writes; empty
//!   means "the engine's `DdlGenerator` derives it from `data_type`" (with
//!   `DdlContext::exact_values`, as M5 import does), so mappers set it only where the generator's
//!   choice would not be the one the fidelity describes.
//! - **Same engine.** A transfer between two servers of the same engine keeps every column's
//!   native type ([`TypeMapper::same_engine`]); the mapper is asked only across engines or for
//!   columns the user overrides.

use oxbase_types::{ColumnDef, DataType, ServerVersion};
use serde::{Deserialize, Serialize};

/// How much of the source values a target type keeps. Ordered from best to worst, so
/// `max` combines two steps (see the module docs).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Fidelity {
    #[default]
    Exact,
    Widened,
    Narrowed,
    Unsupported,
}

impl Fidelity {
    /// The worse of two fidelities (for a chain of steps, or a whole table).
    #[must_use]
    pub fn worst(self, other: Fidelity) -> Fidelity {
        self.max(other)
    }

    /// `true` when values can be lost or refused (`narrowed`, `unsupported`).
    pub fn is_lossy(self) -> bool {
        matches!(self, Self::Narrowed | Self::Unsupported)
    }

    /// The snake_case name (`exact`, `widened`, `narrowed`, `unsupported`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Widened => "widened",
            Self::Narrowed => "narrowed",
            Self::Unsupported => "unsupported",
        }
    }
}

/// How the target engine stores one logical type. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeMapping {
    /// The logical type the target column gets (and the load batches carry).
    pub data_type: DataType,
    /// The type the target `CREATE TABLE` writes; empty = derived by the target's
    /// `DdlGenerator` from `data_type`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub native_type: String,
    pub fidelity: Fidelity,
    /// Why the mapping is not exact (always set unless `fidelity` is `exact`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl TypeMapping {
    /// `data_type` kept exactly, with the generator's native spelling.
    pub fn exact(data_type: DataType) -> Self {
        Self {
            data_type,
            native_type: String::new(),
            fidelity: Fidelity::Exact,
            reason: None,
        }
    }

    /// Stored as `data_type` (spelled `native_type`, empty = derived) with `fidelity` for
    /// `reason`.
    pub fn with(
        data_type: DataType,
        native_type: impl Into<String>,
        fidelity: Fidelity,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            data_type,
            native_type: native_type.into(),
            fidelity,
            reason: Some(reason.into()),
        }
    }

    /// No target type for `data_type`: the column cannot be copied as it is.
    pub fn unsupported(data_type: DataType, reason: impl Into<String>) -> Self {
        Self::with(data_type, String::new(), Fidelity::Unsupported, reason)
    }

    /// Builder: the native spelling.
    #[must_use]
    pub fn native(mut self, native_type: impl Into<String>) -> Self {
        self.native_type = native_type.into();
        self
    }
}

/// Maps column types between one engine and the logical model. One implementation per SQL
/// engine, registered with
/// [`DialectRegistry::register_type_mapper`](crate::DialectRegistry::register_type_mapper).
/// See the module docs for the contract.
pub trait TypeMapper: Send + Sync {
    /// The logical type of `column` as this engine's fetcher read it (source side). Default:
    /// `column.data_type`. Engines refine what their fetchers leave ambiguous (an `Other` type
    /// the engine can still describe, MySQL `tinyint(1)` as `bool`).
    fn logical(&self, column: &ColumnDef, version: &ServerVersion) -> DataType {
        let _ = version;
        column.data_type.clone()
    }

    /// How this engine stores a column of logical type `data_type` on server `version`
    /// (target side). Total over [`DataType`].
    fn map(&self, data_type: &DataType, version: &ServerVersion) -> TypeMapping;

    /// The mapping of `column` when source and target are this same engine: its own native
    /// type, `exact`. Engines override it only when a native type cannot be created as read
    /// (a type that lives in another schema, a server-version gap).
    fn same_engine(&self, column: &ColumnDef, version: &ServerVersion) -> TypeMapping {
        let _ = version;
        TypeMapping::exact(column.data_type.clone()).native(column.native_type.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    struct Fixed;

    impl TypeMapper for Fixed {
        fn map(&self, data_type: &DataType, _version: &ServerVersion) -> TypeMapping {
            match data_type {
                DataType::Uuid => TypeMapping::with(
                    DataType::Char { length: Some(36) },
                    "char(36)",
                    Fidelity::Widened,
                    "uuid as text",
                ),
                other => TypeMapping::exact(other.clone()),
            }
        }
    }

    #[test]
    fn fidelity_orders_from_best_to_worst() {
        assert!(Fidelity::Exact < Fidelity::Widened);
        assert!(Fidelity::Widened < Fidelity::Narrowed);
        assert!(Fidelity::Narrowed < Fidelity::Unsupported);
        assert_eq!(
            Fidelity::Widened.worst(Fidelity::Narrowed),
            Fidelity::Narrowed
        );
        assert_eq!(Fidelity::Exact.worst(Fidelity::Exact), Fidelity::Exact);
        assert!(Fidelity::Narrowed.is_lossy() && !Fidelity::Widened.is_lossy());
        assert_eq!(
            serde_json::to_string(&Fidelity::Unsupported).unwrap(),
            "\"unsupported\""
        );
        assert_eq!(Fidelity::Narrowed.as_str(), "narrowed");
    }

    #[test]
    fn mappings_serialize_in_camel_case_and_default_methods_keep_the_column() {
        let v = ServerVersion::new(8, 4, 13);
        let m = Fixed.map(&DataType::Uuid, &v);
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["nativeType"], "char(36)");
        assert_eq!(json["fidelity"], "widened");
        assert_eq!(json["dataType"]["type"], "char");
        assert_eq!(serde_json::from_value::<TypeMapping>(json).unwrap(), m);

        let exact = serde_json::to_value(TypeMapping::exact(DataType::Text)).unwrap();
        assert!(exact.get("nativeType").is_none() && exact.get("reason").is_none());

        let mut column = ColumnDef::new("id", DataType::INT64);
        column.native_type = "bigint".into();
        assert_eq!(Fixed.logical(&column, &v), DataType::INT64);
        let same = Fixed.same_engine(&column, &v);
        assert_eq!(same.native_type, "bigint");
        assert_eq!(same.fidelity, Fidelity::Exact);
        let refused = TypeMapping::unsupported(DataType::Interval, "no interval type");
        assert_eq!(refused.fidelity, Fidelity::Unsupported);
        assert_eq!(refused.reason.as_deref(), Some("no interval type"));
    }
}
