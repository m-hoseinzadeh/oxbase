//! Object kinds and engine-agnostic object paths.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{Error, Extras};

/// Every kind of database object oxbase models.
///
/// Each engine fills an object-kind x role matrix (see `oxbase-dialect`); adding a kind here adds a
/// row to that matrix. Not every kind exists on every engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    /// A database / catalog (MongoDB database, Redis logical DB index).
    Database,
    Schema,
    /// A table. A MongoDB collection is also a `Table` (there is no separate collection kind).
    Table,
    Column,
    Index,
    ForeignKey,
    Check,
    View,
    MatView,
    Function,
    Procedure,
    Trigger,
    Sequence,
    User,
    Role,
    Grant,
    Event,
    /// A user-defined type (enum, composite, domain, alias type).
    Type,
    Extension,
    /// A key in a key-value store (Redis), addressed by name inside a `Database` (logical DB).
    Key,
}

impl ObjectKind {
    /// Every kind, in a stable order (used for matrices and reports).
    pub const ALL: [ObjectKind; 20] = [
        ObjectKind::Database,
        ObjectKind::Schema,
        ObjectKind::Table,
        ObjectKind::Column,
        ObjectKind::Index,
        ObjectKind::ForeignKey,
        ObjectKind::Check,
        ObjectKind::View,
        ObjectKind::MatView,
        ObjectKind::Function,
        ObjectKind::Procedure,
        ObjectKind::Trigger,
        ObjectKind::Sequence,
        ObjectKind::User,
        ObjectKind::Role,
        ObjectKind::Grant,
        ObjectKind::Event,
        ObjectKind::Type,
        ObjectKind::Extension,
        ObjectKind::Key,
    ];

    /// Stable snake_case identifier (also the serde form).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Database => "database",
            Self::Schema => "schema",
            Self::Table => "table",
            Self::Column => "column",
            Self::Index => "index",
            Self::ForeignKey => "foreign_key",
            Self::Check => "check",
            Self::View => "view",
            Self::MatView => "mat_view",
            Self::Function => "function",
            Self::Procedure => "procedure",
            Self::Trigger => "trigger",
            Self::Sequence => "sequence",
            Self::User => "user",
            Self::Role => "role",
            Self::Grant => "grant",
            Self::Event => "event",
            Self::Type => "type",
            Self::Extension => "extension",
            Self::Key => "key",
        }
    }

    /// Human-readable label.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Database => "Database",
            Self::Schema => "Schema",
            Self::Table => "Table",
            Self::Column => "Column",
            Self::Index => "Index",
            Self::ForeignKey => "Foreign key",
            Self::Check => "Check constraint",
            Self::View => "View",
            Self::MatView => "Materialized view",
            Self::Function => "Function",
            Self::Procedure => "Procedure",
            Self::Trigger => "Trigger",
            Self::Sequence => "Sequence",
            Self::User => "User",
            Self::Role => "Role",
            Self::Grant => "Grant",
            Self::Event => "Event",
            Self::Type => "Type",
            Self::Extension => "Extension",
            Self::Key => "Key",
        }
    }

    /// `true` for kinds that live inside a table (or a view / materialized view, for indexes and
    /// triggers) and are addressed through [`ObjectRef::parent`]: columns, indexes, foreign keys,
    /// checks and triggers.
    pub const fn is_table_child(self) -> bool {
        matches!(
            self,
            Self::Column | Self::Index | Self::ForeignKey | Self::Check | Self::Trigger
        )
    }

    /// `true` for server-level security objects that do not live in a schema.
    pub const fn is_security(self) -> bool {
        matches!(self, Self::User | Self::Role | Self::Grant)
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ObjectKind {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        Self::ALL
            .into_iter()
            .find(|k| k.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| Error::invalid_input(format!("unknown object kind '{s}'")))
    }
}

/// Engine-agnostic path to one database object.
///
/// - `catalog`: the database (PostgreSQL database, MySQL schema-as-database, SQL Server database,
///   SQLite attached database name, MongoDB database). `None` means "the connection's current".
/// - `schema`: the namespace inside the catalog (PostgreSQL / SQL Server schema). `None` for
///   engines without one (MySQL, SQLite, MongoDB) or for objects above schema level.
/// - `parent`: the owning object's name for table children ([`ObjectKind::is_table_child`]),
///   else `None`. The owner is usually a table, but can be a view or materialized view (indexes on
///   PostgreSQL materialized views, `INSTEAD OF` triggers on views).
/// - `parent_kind`: the owner's kind; `None` means [`ObjectKind::Table`]. See [`ObjectRef::owner`].
/// - `name`: the object's own name. Empty for [`ObjectRef::root`], and for unnamed constraints
///   (a foreign key or check with no name, common in SQLite) as produced by
///   `ObjectDef::object_ref`. Such a reference cannot be looked up by name; address the
///   constraint through its table's definition instead.
///
/// Names are stored unquoted and exactly as the server reports them; quoting is a dialect job.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjectRef {
    pub kind: ObjectKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_kind: Option<ObjectKind>,
    pub name: String,
}

impl ObjectRef {
    /// A reference with only a kind and a name.
    pub fn new(kind: ObjectKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            catalog: None,
            schema: None,
            parent: None,
            parent_kind: None,
            name: name.into(),
        }
    }

    /// The server itself: the parent passed to a `Database` fetcher's `list`.
    ///
    /// It has kind `Database`, an empty name and no catalog or schema; see [`ObjectRef::is_root`].
    pub fn root() -> Self {
        Self::new(ObjectKind::Database, "")
    }

    /// `true` for [`ObjectRef::root`].
    pub fn is_root(&self) -> bool {
        self.kind == ObjectKind::Database
            && self.name.is_empty()
            && self.catalog.is_none()
            && self.schema.is_none()
            && self.parent.is_none()
            && self.parent_kind.is_none()
    }

    /// A database / catalog.
    pub fn database(name: impl Into<String>) -> Self {
        Self::new(ObjectKind::Database, name)
    }

    /// A schema inside an optional catalog.
    pub fn schema(catalog: Option<&str>, name: impl Into<String>) -> Self {
        Self::new(ObjectKind::Schema, name).in_catalog(catalog)
    }

    /// A table in an optional catalog and schema.
    pub fn table(catalog: Option<&str>, schema: Option<&str>, name: impl Into<String>) -> Self {
        Self::new(ObjectKind::Table, name)
            .in_catalog(catalog)
            .in_schema(schema)
    }

    /// Sets the catalog.
    #[must_use]
    pub fn in_catalog(mut self, catalog: Option<&str>) -> Self {
        self.catalog = catalog.map(str::to_owned);
        self
    }

    /// Sets the schema.
    #[must_use]
    pub fn in_schema(mut self, schema: Option<&str>) -> Self {
        self.schema = schema.map(str::to_owned);
        self
    }

    /// A reference of `kind` named `name` that lives inside this object.
    ///
    /// For table children (`kind.is_table_child()` and `self` is a table, view or other
    /// schema-level object), catalog and schema are inherited, `parent` is set to `self`'s name
    /// and `parent_kind` to `self`'s kind. When `self` is a database the child goes in that
    /// catalog; when `self` is a schema the child goes in that schema; anything else inherits
    /// `self`'s catalog and schema.
    #[must_use]
    pub fn child(&self, kind: ObjectKind, name: impl Into<String>) -> Self {
        let mut r = Self::new(kind, name);
        match self.kind {
            _ if self.is_root() => {}
            ObjectKind::Database => r.catalog = Some(self.name.clone()),
            ObjectKind::Schema => {
                r.catalog.clone_from(&self.catalog);
                r.schema = Some(self.name.clone());
            }
            _ => {
                r.catalog.clone_from(&self.catalog);
                r.schema.clone_from(&self.schema);
                if kind.is_table_child() {
                    r.parent = Some(self.name.clone());
                    r.parent_kind = Some(self.kind);
                }
            }
        }
        r
    }

    /// For a table child, a reference to its owner: a table, or a view / materialized view when
    /// [`ObjectRef::parent_kind`] says so (`None` means table).
    pub fn owner(&self) -> Option<ObjectRef> {
        let parent = self.parent.as_deref()?;
        Some(
            Self::new(self.parent_kind.unwrap_or(ObjectKind::Table), parent)
                .in_catalog(self.catalog.as_deref())
                .in_schema(self.schema.as_deref()),
        )
    }

    /// Non-empty path parts in order: catalog, schema, parent, name.
    pub fn path(&self) -> Vec<&str> {
        [
            self.catalog.as_deref(),
            self.schema.as_deref(),
            self.parent.as_deref(),
            Some(self.name.as_str()),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect()
    }
}

impl fmt::Display for ObjectRef {
    /// `kind:catalog.schema.parent.name` for logs (unquoted; not SQL).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.path().join("."))
    }
}

/// One row of an object listing (the object tree, search results, the schema cache).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectSummary {
    #[serde(rename = "ref")]
    pub obj: ObjectRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Approximate row count from statistics, for tables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_estimate: Option<u64>,
    /// `true` for objects owned by the system (catalog tables, built-in schemas).
    #[serde(default)]
    pub system: bool,
    /// Cheap engine-specific facts known at listing time (table engine, size, ...).
    #[serde(default, skip_serializing_if = "Extras::is_empty")]
    pub extras: Extras,
}

impl ObjectSummary {
    pub fn new(obj: ObjectRef) -> Self {
        Self {
            obj,
            comment: None,
            row_estimate: None,
            system: false,
            extras: Extras::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn kind_names_round_trip() {
        for k in ObjectKind::ALL {
            assert_eq!(k.as_str().parse::<ObjectKind>().unwrap(), k);
            assert_eq!(
                serde_json::to_string(&k).unwrap(),
                format!("\"{}\"", k.as_str())
            );
        }
        assert!("synonym".parse::<ObjectKind>().is_err());
        // ALL has no duplicates.
        let mut sorted = ObjectKind::ALL.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ObjectKind::ALL.len());
    }

    #[test]
    fn child_paths() {
        let db = ObjectRef::database("shop");
        let schema = db.child(ObjectKind::Schema, "public");
        assert_eq!(schema, ObjectRef::schema(Some("shop"), "public"));
        let table = schema.child(ObjectKind::Table, "orders");
        assert_eq!(
            table,
            ObjectRef::table(Some("shop"), Some("public"), "orders")
        );
        let col = table.child(ObjectKind::Column, "id");
        assert_eq!(col.parent.as_deref(), Some("orders"));
        assert_eq!(col.parent_kind, Some(ObjectKind::Table));
        assert_eq!(col.owner(), Some(table.clone()));
        assert_eq!(col.path(), ["shop", "public", "orders", "id"]);
        assert_eq!(col.to_string(), "column:shop.public.orders.id");
        // A view next to the table does not get a parent.
        let view = table.child(ObjectKind::View, "v");
        assert_eq!(view.parent, None);
        assert_eq!(
            ObjectRef::root().child(ObjectKind::Database, "x"),
            ObjectRef::database("x")
        );
        assert!(ObjectRef::root().is_root());
        assert!(!db.is_root());
    }

    #[test]
    fn children_of_views_keep_the_owner_kind() {
        let v = ObjectRef::new(ObjectKind::View, "v").in_schema(Some("s"));
        let trg = v.child(ObjectKind::Trigger, "trg");
        assert_eq!(trg.parent.as_deref(), Some("v"));
        assert_eq!(trg.owner(), Some(v));
        let mv = ObjectRef::new(ObjectKind::MatView, "daily").in_schema(Some("s"));
        assert_eq!(
            mv.child(ObjectKind::Index, "ix").owner().map(|o| o.kind),
            Some(ObjectKind::MatView)
        );
        // A ref without parent_kind (older JSON) is owned by a table.
        let legacy: ObjectRef =
            serde_json::from_str(r#"{"kind":"index","schema":"s","parent":"t","name":"ix"}"#)
                .unwrap();
        assert_eq!(legacy.owner(), Some(ObjectRef::table(None, Some("s"), "t")));
    }

    #[test]
    fn serde_shape() {
        let t = ObjectRef::table(None, Some("dbo"), "t");
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, r#"{"kind":"table","schema":"dbo","name":"t"}"#);
        assert_eq!(serde_json::from_str::<ObjectRef>(&json).unwrap(), t);
        let s = ObjectSummary::new(t);
        let back: ObjectSummary =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
