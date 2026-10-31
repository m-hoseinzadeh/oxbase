//! PostgreSQL type -> Arrow type mapping.
//!
//! | PostgreSQL | Arrow |
//! |---|---|
//! | `bool` | `Boolean` |
//! | `int2` / `int4` / `int8` | `Int16` / `Int32` / `Int64` |
//! | `oid` | `UInt32` |
//! | `float4` / `float8` | `Float32` / `Float64` |
//! | `numeric(p, s)` with `p <= 38` and `s <= p` | `Decimal128(p, s)` |
//! | `numeric` without a fitting typmod | `Utf8` (the exact text) |
//! | `text`, `varchar`, `bpchar`, `name`, `"char"`, `unknown` | `Utf8` |
//! | `json` / `jsonb` | `Utf8` with the canonical `arrow.json` extension metadata |
//! | `bytea` | `Binary` |
//! | `date` | `Date32` |
//! | `time` | `Time64(Microsecond)` |
//! | `timestamp` | `Timestamp(Microsecond, None)` |
//! | `timestamptz` | `Timestamp(Microsecond, "UTC")` |
//! | `interval` | `Utf8` (the server's `postgres` style text) |
//! | `uuid` | `FixedSizeBinary(16)` with the canonical `arrow.uuid` extension metadata |
//! | arrays, ranges, enums, composites, everything else | `Utf8` (the server's text form) |
//!
//! Every field is nullable and carries `oxbase.pg.type` (type name, schema-qualified outside
//! `pg_catalog`) and `oxbase.pg.oid` metadata. Domains map like their base type.

use std::collections::HashMap;

use arrow_schema::{DataType, Field, TimeUnit};
use tokio_postgres::types::{Kind, Type};

/// Field metadata key: the PostgreSQL type name.
pub const META_PG_TYPE: &str = "oxbase.pg.type";
/// Field metadata key: the PostgreSQL type OID.
pub const META_PG_OID: &str = "oxbase.pg.oid";
/// Arrow canonical extension name key.
pub const META_EXTENSION_NAME: &str = "ARROW:extension:name";
/// Arrow canonical extension metadata key.
pub const META_EXTENSION_METADATA: &str = "ARROW:extension:metadata";

/// How a column is decoded and which Arrow type it becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColKind {
    Bool,
    Int2,
    Int4,
    Int8,
    Oid,
    Float4,
    Float8,
    /// `numeric(p, s)` that fits `Decimal128`.
    Decimal {
        precision: u8,
        scale: i8,
    },
    /// Any other `numeric`: exact text.
    Numeric,
    /// Text-like types whose binary form is the text itself.
    Text,
    Json,
    Jsonb,
    Bytea,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Interval,
    Uuid,
    /// Anything else: the text form as `Utf8`.
    Other,
}

impl ColKind {
    /// The kind for a column of type `ty` with type modifier `typmod` (-1 when absent).
    pub(crate) fn of(ty: &Type, typmod: i32) -> Self {
        if let Kind::Domain(base) = ty.kind() {
            return Self::of(base, typmod);
        }
        match *ty {
            Type::BOOL => Self::Bool,
            Type::INT2 => Self::Int2,
            Type::INT4 => Self::Int4,
            Type::INT8 => Self::Int8,
            Type::OID => Self::Oid,
            Type::FLOAT4 => Self::Float4,
            Type::FLOAT8 => Self::Float8,
            Type::NUMERIC => numeric_kind(typmod),
            Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => Self::Text,
            Type::JSON => Self::Json,
            Type::JSONB => Self::Jsonb,
            Type::BYTEA => Self::Bytea,
            Type::DATE => Self::Date,
            Type::TIME => Self::Time,
            Type::TIMESTAMP => Self::Timestamp,
            Type::TIMESTAMPTZ => Self::TimestampTz,
            Type::INTERVAL => Self::Interval,
            Type::UUID => Self::Uuid,
            _ => Self::Other,
        }
    }

    pub(crate) fn arrow_type(self) -> DataType {
        match self {
            Self::Bool => DataType::Boolean,
            Self::Int2 => DataType::Int16,
            Self::Int4 => DataType::Int32,
            Self::Int8 => DataType::Int64,
            Self::Oid => DataType::UInt32,
            Self::Float4 => DataType::Float32,
            Self::Float8 => DataType::Float64,
            Self::Decimal { precision, scale } => DataType::Decimal128(precision, scale),
            Self::Bytea => DataType::Binary,
            Self::Date => DataType::Date32,
            Self::Time => DataType::Time64(TimeUnit::Microsecond),
            Self::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
            Self::TimestampTz => DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            Self::Uuid => DataType::FixedSizeBinary(16),
            Self::Numeric
            | Self::Text
            | Self::Json
            | Self::Jsonb
            | Self::Interval
            | Self::Other => DataType::Utf8,
        }
    }
}

/// `numeric(p, s)` typmod: `((p << 16) | (s & 0x7ff)) + 4`; the scale is an 11-bit signed number
/// (negative scales and scales above the precision exist since PostgreSQL 15).
pub(crate) fn numeric_precision_scale(typmod: i32) -> Option<(i32, i32)> {
    if typmod < 4 {
        return None;
    }
    let t = typmod - 4;
    let precision = (t >> 16) & 0xffff;
    let scale = ((t & 0x7ff) ^ 1024) - 1024;
    Some((precision, scale))
}

fn numeric_kind(typmod: i32) -> ColKind {
    match numeric_precision_scale(typmod) {
        Some((p, s)) if (1..=38).contains(&p) && s <= p && s >= -38 => ColKind::Decimal {
            precision: u8::try_from(p).unwrap_or(38),
            scale: i8::try_from(s).unwrap_or(0),
        },
        _ => ColKind::Numeric,
    }
}

/// A column of a result set: name, PostgreSQL type (unknown in untyped results) and kind.
#[derive(Debug, Clone)]
pub(crate) struct ColSpec {
    pub name: String,
    pub ty: Option<Type>,
    pub kind: ColKind,
}

impl ColSpec {
    pub(crate) fn typed(name: &str, ty: &Type, typmod: i32) -> Self {
        Self {
            name: name.to_owned(),
            ty: Some(ty.clone()),
            kind: ColKind::of(ty, typmod),
        }
    }

    /// A column whose type is unknown (multi-statement scripts): its text as `Utf8`.
    pub(crate) fn untyped(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ty: None,
            kind: ColKind::Other,
        }
    }

    /// The type name shown in metadata and warnings.
    pub(crate) fn type_name(&self) -> String {
        self.ty
            .as_ref()
            .map_or_else(|| "unknown".to_owned(), type_name)
    }

    pub(crate) fn arrow_field(&self) -> Field {
        let mut meta = HashMap::new();
        if let Some(ty) = &self.ty {
            meta.insert(META_PG_TYPE.to_owned(), type_name(ty));
            meta.insert(META_PG_OID.to_owned(), ty.oid().to_string());
        }
        match self.kind {
            ColKind::Json | ColKind::Jsonb => {
                meta.insert(META_EXTENSION_NAME.to_owned(), "arrow.json".to_owned());
                meta.insert(META_EXTENSION_METADATA.to_owned(), "{}".to_owned());
            }
            ColKind::Uuid => {
                meta.insert(META_EXTENSION_NAME.to_owned(), "arrow.uuid".to_owned());
            }
            _ => {}
        }
        Field::new(&self.name, self.kind.arrow_type(), true).with_metadata(meta)
    }
}

/// `name`, or `schema.name` outside `pg_catalog` / `public`.
pub(crate) fn type_name(ty: &Type) -> String {
    match ty.schema() {
        "pg_catalog" | "public" | "" => ty.name().to_owned(),
        schema => format!("{schema}.{}", ty.name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_types() {
        let cases = [
            (Type::BOOL, DataType::Boolean),
            (Type::INT2, DataType::Int16),
            (Type::INT4, DataType::Int32),
            (Type::INT8, DataType::Int64),
            (Type::OID, DataType::UInt32),
            (Type::FLOAT4, DataType::Float32),
            (Type::FLOAT8, DataType::Float64),
            (Type::TEXT, DataType::Utf8),
            (Type::VARCHAR, DataType::Utf8),
            (Type::BPCHAR, DataType::Utf8),
            (Type::NAME, DataType::Utf8),
            (Type::BYTEA, DataType::Binary),
            (Type::DATE, DataType::Date32),
            (Type::TIME, DataType::Time64(TimeUnit::Microsecond)),
            (
                Type::TIMESTAMP,
                DataType::Timestamp(TimeUnit::Microsecond, None),
            ),
            (
                Type::TIMESTAMPTZ,
                DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            ),
            (Type::INTERVAL, DataType::Utf8),
            (Type::UUID, DataType::FixedSizeBinary(16)),
            (Type::JSON, DataType::Utf8),
            (Type::JSONB, DataType::Utf8),
            (Type::INT4_ARRAY, DataType::Utf8),
            (Type::INET, DataType::Utf8),
            (Type::TIMETZ, DataType::Utf8),
            (Type::NUMERIC, DataType::Utf8),
        ];
        for (ty, want) in cases {
            assert_eq!(ColKind::of(&ty, -1).arrow_type(), want, "{ty}");
        }
    }

    #[test]
    fn numeric_typmods() {
        let typmod = |p: i32, s: i32| ((p << 16) | (s & 0x7ff)) + 4;
        assert_eq!(numeric_precision_scale(typmod(10, 2)), Some((10, 2)));
        assert_eq!(numeric_precision_scale(typmod(5, -3)), Some((5, -3)));
        assert_eq!(numeric_precision_scale(-1), None);
        assert_eq!(
            ColKind::of(&Type::NUMERIC, typmod(10, 2)).arrow_type(),
            DataType::Decimal128(10, 2)
        );
        assert_eq!(
            ColKind::of(&Type::NUMERIC, typmod(38, 0)).arrow_type(),
            DataType::Decimal128(38, 0)
        );
        assert_eq!(
            ColKind::of(&Type::NUMERIC, typmod(5, -3)).arrow_type(),
            DataType::Decimal128(5, -3)
        );
        // Too wide for Decimal128, or scale above precision: exact text.
        assert_eq!(
            ColKind::of(&Type::NUMERIC, typmod(39, 2)).arrow_type(),
            DataType::Utf8
        );
        assert_eq!(
            ColKind::of(&Type::NUMERIC, typmod(2, 5)).arrow_type(),
            DataType::Utf8
        );
    }

    #[test]
    fn domains_map_like_their_base_type() {
        let domain = Type::new(
            "positive_int".into(),
            90_001,
            Kind::Domain(Type::INT4),
            "app".into(),
        );
        assert_eq!(ColKind::of(&domain, -1), ColKind::Int4);
        let spec = ColSpec::typed("n", &domain, -1);
        let field = spec.arrow_field();
        assert_eq!(field.data_type(), &DataType::Int32);
        assert_eq!(
            field.metadata().get(META_PG_TYPE).map(String::as_str),
            Some("app.positive_int")
        );
        assert_eq!(
            field.metadata().get(META_PG_OID).map(String::as_str),
            Some("90001")
        );
    }

    #[test]
    fn json_and_uuid_carry_extension_metadata() {
        let json = ColSpec::typed("doc", &Type::JSONB, -1).arrow_field();
        assert_eq!(json.data_type(), &DataType::Utf8);
        assert_eq!(
            json.metadata().get(META_EXTENSION_NAME).map(String::as_str),
            Some("arrow.json")
        );
        assert_eq!(
            json.metadata().get(META_PG_TYPE).map(String::as_str),
            Some("jsonb")
        );
        let id = ColSpec::typed("id", &Type::UUID, -1).arrow_field();
        assert_eq!(
            id.metadata().get(META_EXTENSION_NAME).map(String::as_str),
            Some("arrow.uuid")
        );
        let untyped = ColSpec::untyped("x").arrow_field();
        assert!(untyped.metadata().is_empty());
        assert!(untyped.is_nullable());
    }
}
