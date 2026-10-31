//! The typed object model: one definition struct per object kind.
//!
//! This is the minimal M0 model. Tables, columns, indexes, foreign keys, checks and views are
//! fully typed; other kinds use [`GenericDef`] (name + native definition text + extras) until
//! they get their own struct. Every definition carries an [`Extras`] map for database-specific
//! properties, which the dialect `Capabilities` trait gates by engine and server version.

use serde::{Deserialize, Serialize};

use crate::{DataType, Extras, ObjectKind, ObjectRef};

/// A typed definition of one database object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObjectDef {
    Table(TableDef),
    /// A column on its own (`ALTER TABLE ... ADD COLUMN`), with its owning table.
    Column {
        table: ObjectRef,
        column: ColumnDef,
    },
    Index {
        table: ObjectRef,
        index: IndexDef,
    },
    ForeignKey {
        table: ObjectRef,
        foreign_key: ForeignKeyDef,
    },
    Check {
        table: ObjectRef,
        check: CheckDef,
    },
    View(ViewDef),
    /// Any kind without a dedicated struct yet (function, trigger, sequence, ...).
    Generic(GenericDef),
}

impl ObjectDef {
    /// The kind of object this definition describes.
    pub fn kind(&self) -> ObjectKind {
        match self {
            Self::Table(_) => ObjectKind::Table,
            Self::Column { .. } => ObjectKind::Column,
            Self::Index { .. } => ObjectKind::Index,
            Self::ForeignKey { .. } => ObjectKind::ForeignKey,
            Self::Check { .. } => ObjectKind::Check,
            Self::View(v) => v.obj.kind,
            Self::Generic(g) => g.obj.kind,
        }
    }

    /// The object's own name (may be empty for unnamed constraints).
    pub fn name(&self) -> &str {
        match self {
            Self::Table(t) => &t.obj.name,
            Self::Column { column, .. } => &column.name,
            Self::Index { index, .. } => &index.name,
            Self::ForeignKey { foreign_key, .. } => foreign_key.name.as_deref().unwrap_or(""),
            Self::Check { check, .. } => check.name.as_deref().unwrap_or(""),
            Self::View(v) => &v.obj.name,
            Self::Generic(g) => &g.obj.name,
        }
    }

    /// The path of the object this definition describes.
    pub fn object_ref(&self) -> ObjectRef {
        match self {
            Self::Table(t) => t.obj.clone(),
            Self::View(v) => v.obj.clone(),
            Self::Generic(g) => g.obj.clone(),
            Self::Column { table, .. }
            | Self::Index { table, .. }
            | Self::ForeignKey { table, .. }
            | Self::Check { table, .. } => table.child(self.kind(), self.name()),
        }
    }

    /// Database-specific properties of this object.
    pub fn extras(&self) -> &Extras {
        match self {
            Self::Table(t) => &t.extras,
            Self::Column { column, .. } => &column.extras,
            Self::Index { index, .. } => &index.extras,
            Self::ForeignKey { foreign_key, .. } => &foreign_key.extras,
            Self::Check { check, .. } => &check.extras,
            Self::View(v) => &v.extras,
            Self::Generic(g) => &g.extras,
        }
    }

    /// Mutable access to the extras map.
    pub fn extras_mut(&mut self) -> &mut Extras {
        match self {
            Self::Table(t) => &mut t.extras,
            Self::Column { column, .. } => &mut column.extras,
            Self::Index { index, .. } => &mut index.extras,
            Self::ForeignKey { foreign_key, .. } => &mut foreign_key.extras,
            Self::Check { check, .. } => &mut check.extras,
            Self::View(v) => &mut v.extras,
            Self::Generic(g) => &mut g.extras,
        }
    }
}

/// A table and everything defined inline with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableDef {
    #[serde(rename = "ref")]
    pub obj: ObjectRef,
    /// Columns in ordinal order.
    pub columns: Vec<ColumnDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_key: Option<PrimaryKeyDef>,
    /// Indexes and unique constraints (not the primary key).
    #[serde(default)]
    pub indexes: Vec<IndexDef>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKeyDef>,
    #[serde(default)]
    pub checks: Vec<CheckDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(default)]
    pub extras: Extras,
}

impl TableDef {
    pub fn new(obj: ObjectRef) -> Self {
        Self {
            obj,
            columns: Vec::new(),
            primary_key: None,
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            checks: Vec::new(),
            comment: None,
            extras: Extras::default(),
        }
    }

    /// Looks up a column by exact name.
    pub fn column(&self, name: &str) -> Option<&ColumnDef> {
        self.columns.iter().find(|c| c.name == name)
    }
}

/// One table column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    /// Engine-neutral type.
    pub data_type: DataType,
    /// The type exactly as the engine spells it (`character varying(20)`, `int(11) unsigned`).
    /// Empty when the definition was built by hand and the dialect should derive it.
    #[serde(default)]
    pub native_type: String,
    pub nullable: bool,
    /// Default as an engine-native SQL expression (`now()`, `'x'::text`, `0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Auto-increment / serial / identity column.
    #[serde(default)]
    pub auto_increment: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated: Option<GeneratedColumn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(default)]
    pub extras: Extras,
}

impl ColumnDef {
    /// A nullable column with no default.
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
            native_type: String::new(),
            nullable: true,
            default: None,
            auto_increment: false,
            generated: None,
            collation: None,
            comment: None,
            extras: Extras::default(),
        }
    }

    /// Builder: marks the column `NOT NULL`.
    #[must_use]
    pub fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }
}

/// A computed column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedColumn {
    /// Engine-native expression.
    pub expression: String,
    /// `true` for stored / persisted, `false` for virtual.
    pub stored: bool,
}

/// A primary key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrimaryKeyDef {
    /// Constraint name, when the engine names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub columns: Vec<String>,
}

/// An index or a unique constraint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexDef {
    pub name: String,
    pub columns: Vec<IndexColumn>,
    #[serde(default)]
    pub unique: bool,
    /// `true` when this is a declared `UNIQUE` constraint rather than a plain index.
    #[serde(default)]
    pub is_constraint: bool,
    /// Partial-index predicate (`WHERE ...`), engine-native.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate: Option<String>,
    #[serde(default)]
    pub extras: Extras,
}

/// One key part of an index: a column or an expression.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexColumn {
    /// Column name, or an engine-native expression when `is_expression` is set.
    pub name: String,
    #[serde(default)]
    pub is_expression: bool,
    #[serde(default)]
    pub descending: bool,
    /// Prefix length (MySQL `col(10)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u32>,
}

impl IndexColumn {
    pub fn column(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_expression: false,
            descending: false,
            length: None,
        }
    }
}

/// Referential action of a foreign key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FkAction {
    #[default]
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

/// A foreign key constraint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForeignKeyDef {
    /// Constraint name; `None` for unnamed constraints (common in SQLite).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub ref_table: ObjectRef,
    pub ref_columns: Vec<String>,
    #[serde(default)]
    pub on_delete: FkAction,
    #[serde(default)]
    pub on_update: FkAction,
    #[serde(default)]
    pub extras: Extras,
}

/// A check constraint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckDef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Engine-native boolean expression.
    pub expression: String,
    #[serde(default)]
    pub extras: Extras,
}

/// A view or materialized view (`obj.kind` is `View` or `MatView`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewDef {
    #[serde(rename = "ref")]
    pub obj: ObjectRef,
    /// The query text (the body after `AS`), engine-native.
    pub definition: String,
    /// Explicit column list, if the view declares one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(default)]
    pub extras: Extras,
}

/// Placeholder model for kinds that have no dedicated struct yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenericDef {
    #[serde(rename = "ref")]
    pub obj: ObjectRef,
    /// Full engine-native definition text (for example `CREATE FUNCTION ...`), when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(default)]
    pub extras: Extras,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::props;
    use pretty_assertions::assert_eq;

    fn sample() -> TableDef {
        let obj = ObjectRef::table(None, Some("public"), "orders");
        let mut t = TableDef::new(obj);
        t.columns
            .push(ColumnDef::new("id", DataType::INT64).not_null());
        t.columns
            .push(ColumnDef::new("customer_id", DataType::INT64));
        t.primary_key = Some(PrimaryKeyDef {
            name: Some("orders_pkey".into()),
            columns: vec!["id".into()],
        });
        t.foreign_keys.push(ForeignKeyDef {
            name: Some("orders_customer_fk".into()),
            columns: vec!["customer_id".into()],
            ref_table: ObjectRef::table(None, Some("public"), "customers"),
            ref_columns: vec!["id".into()],
            on_delete: FkAction::Cascade,
            on_update: FkAction::NoAction,
            extras: Extras::new().with(props::DEFERRABLE, false),
        });
        t.extras.insert(props::UNLOGGED, true);
        t
    }

    #[test]
    fn accessors() {
        let t = sample();
        assert!(t.column("id").is_some_and(|c| !c.nullable));
        let def = ObjectDef::Table(t.clone());
        assert_eq!(def.kind(), ObjectKind::Table);
        assert_eq!(def.name(), "orders");
        assert_eq!(
            def.extras().get(&props::UNLOGGED),
            Some(&crate::Value::Bool(true))
        );

        let col = ObjectDef::Column {
            table: t.obj.clone(),
            column: t.columns[0].clone(),
        };
        let r = col.object_ref();
        assert_eq!(r.kind, ObjectKind::Column);
        assert_eq!(r.parent.as_deref(), Some("orders"));
        assert_eq!(r.schema.as_deref(), Some("public"));

        let fk = ObjectDef::ForeignKey {
            table: t.obj.clone(),
            foreign_key: t.foreign_keys[0].clone(),
        };
        assert_eq!(fk.object_ref().name, "orders_customer_fk");
    }

    #[test]
    fn serde_round_trip() {
        let def = ObjectDef::Table(sample());
        let json = serde_json::to_string_pretty(&def).unwrap();
        let back: ObjectDef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, def);

        let mv = ObjectDef::View(ViewDef {
            obj: ObjectRef::new(ObjectKind::MatView, "daily"),
            definition: "SELECT 1".into(),
            columns: vec![],
            comment: None,
            extras: Extras::default(),
        });
        assert_eq!(mv.kind(), ObjectKind::MatView);
        let back: ObjectDef = serde_json::from_str(&serde_json::to_string(&mv).unwrap()).unwrap();
        assert_eq!(back, mv);
    }
}
