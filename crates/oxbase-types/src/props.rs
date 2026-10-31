//! Property identifiers and the typed extras map.
//!
//! A [`PropId`] names one property of an object: either a core field of the typed model
//! (`nullable`, `default`, ...) or a database-specific extra (`tablespace`, `without_rowid`,
//! ...). Extras live in an [`Extras`] map on each definition; whether an engine and server version
//! support a given extra is answered by the dialect `Capabilities` trait.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::Value;

/// Name of one object property. Cheap to clone for the well-known constants in this module.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PropId(Cow<'static, str>);

impl PropId {
    /// A property id from a static string (usable in `const`).
    pub const fn from_static(name: &'static str) -> Self {
        Self(Cow::Borrowed(name))
    }

    /// A property id from an owned string (engine crates defining their own extras at runtime).
    pub fn new(name: impl Into<String>) -> Self {
        Self(Cow::Owned(name.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PropId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&'static str> for PropId {
    fn from(s: &'static str) -> Self {
        Self::from_static(s)
    }
}

// --- Core model properties (used by comparators to name what changed) ---

pub const NAME: PropId = PropId::from_static("name");
pub const COMMENT: PropId = PropId::from_static("comment");
pub const OWNER: PropId = PropId::from_static("owner");
pub const DATA_TYPE: PropId = PropId::from_static("data_type");
pub const NULLABLE: PropId = PropId::from_static("nullable");
pub const DEFAULT: PropId = PropId::from_static("default");
pub const AUTO_INCREMENT: PropId = PropId::from_static("auto_increment");
pub const GENERATED: PropId = PropId::from_static("generated");
pub const COLLATION: PropId = PropId::from_static("collation");
pub const POSITION: PropId = PropId::from_static("position");
pub const PRIMARY_KEY: PropId = PropId::from_static("primary_key");
pub const COLUMNS: PropId = PropId::from_static("columns");
pub const UNIQUE: PropId = PropId::from_static("unique");
pub const PREDICATE: PropId = PropId::from_static("predicate");
pub const REF_TABLE: PropId = PropId::from_static("ref_table");
pub const REF_COLUMNS: PropId = PropId::from_static("ref_columns");
pub const ON_DELETE: PropId = PropId::from_static("on_delete");
pub const ON_UPDATE: PropId = PropId::from_static("on_update");
pub const EXPRESSION: PropId = PropId::from_static("expression");
pub const DEFINITION: PropId = PropId::from_static("definition");
/// M3: an index is a declared `UNIQUE` constraint rather than a plain index
/// (`IndexDef::is_constraint`).
pub const CONSTRAINT: PropId = PropId::from_static("constraint");

// --- Common database-specific extras (gated by `Capabilities`) ---

/// Character set (MySQL/MariaDB table or column).
pub const CHARSET: PropId = PropId::from_static("charset");
/// Storage engine (MySQL/MariaDB `ENGINE=`).
pub const STORAGE_ENGINE: PropId = PropId::from_static("storage_engine");
/// Row format (MySQL/MariaDB `ROW_FORMAT=`).
pub const ROW_FORMAT: PropId = PropId::from_static("row_format");
/// Tablespace (PostgreSQL, MySQL).
pub const TABLESPACE: PropId = PropId::from_static("tablespace");
/// `UNLOGGED` table (PostgreSQL).
pub const UNLOGGED: PropId = PropId::from_static("unlogged");
/// `WITHOUT ROWID` table (SQLite).
pub const WITHOUT_ROWID: PropId = PropId::from_static("without_rowid");
/// `STRICT` table (SQLite 3.37+).
pub const STRICT: PropId = PropId::from_static("strict");
/// Partitioning clause, as engine-native text.
pub const PARTITION_BY: PropId = PropId::from_static("partition_by");
/// Index access method (`btree`, `gin`, `hash`, ...).
pub const INDEX_METHOD: PropId = PropId::from_static("index_method");
/// Covering-index payload columns (`INCLUDE (...)`), as a JSON array of names.
pub const INCLUDE_COLUMNS: PropId = PropId::from_static("include_columns");
/// Clustered index (SQL Server).
pub const CLUSTERED: PropId = PropId::from_static("clustered");
/// Index or table fill factor.
pub const FILL_FACTOR: PropId = PropId::from_static("fill_factor");
/// Deferrable constraint (PostgreSQL, SQLite).
pub const DEFERRABLE: PropId = PropId::from_static("deferrable");
/// Initially deferred constraint (PostgreSQL, SQLite).
pub const INITIALLY_DEFERRED: PropId = PropId::from_static("initially_deferred");
/// View `WITH CHECK OPTION` mode (`local` / `cascaded`).
pub const CHECK_OPTION: PropId = PropId::from_static("check_option");
/// View or routine `SQL SECURITY` / `SECURITY DEFINER` mode.
pub const SECURITY: PropId = PropId::from_static("security");
/// Identity seed / increment (SQL Server, PostgreSQL identity), as a JSON object.
pub const IDENTITY: PropId = PropId::from_static("identity");

// --- M2: routines, databases, views, listings ---

/// Database or schema encoding (PostgreSQL `UTF8`).
pub const ENCODING: PropId = PropId::from_static("encoding");
/// Routine is deterministic (MySQL/MariaDB `DETERMINISTIC`).
pub const DETERMINISTIC: PropId = PropId::from_static("deterministic");
/// Routine volatility (PostgreSQL `immutable` / `stable` / `volatile`).
pub const VOLATILITY: PropId = PropId::from_static("volatility");
/// Routine strictness (PostgreSQL `STRICT` / `RETURNS NULL ON NULL INPUT`).
pub const STRICT_INPUT: PropId = PropId::from_static("strict_input");
/// Routine data access (MySQL `CONTAINS SQL`, `READS SQL DATA`, `MODIFIES SQL DATA`, `NO SQL`).
pub const SQL_DATA_ACCESS: PropId = PropId::from_static("sql_data_access");
/// Definer of a view, routine, trigger or event (MySQL/MariaDB `user@host`).
pub const DEFINER: PropId = PropId::from_static("definer");
/// View algorithm (MySQL/MariaDB `MERGE` / `TEMPTABLE` / `UNDEFINED`).
pub const VIEW_ALGORITHM: PropId = PropId::from_static("view_algorithm");
/// Materialized view holds data (PostgreSQL `ispopulated`).
pub const POPULATED: PropId = PropId::from_static("populated");
/// Temporary object (SQLite `temp` schema, PostgreSQL temporary tables).
pub const TEMPORARY: PropId = PropId::from_static("temporary");
/// Engine-specific kind of a table-like object (`partitioned`, `foreign`, `virtual`, ...).
pub const TABLE_TYPE: PropId = PropId::from_static("table_type");
/// File behind a database (SQLite `main` / attached database file path).
pub const FILE: PropId = PropId::from_static("file");

// --- M2: listing facts (`ObjectSummary::extras`) ---

/// Column summary: the type as the engine spells it (text).
pub const NATIVE_TYPE: PropId = PropId::from_static("native_type");
/// Database summary: this is the session's current database (bool).
pub const CURRENT: PropId = PropId::from_static("current");
/// Total size on disk in bytes, when cheap to know (int).
pub const SIZE_BYTES: PropId = PropId::from_static("size_bytes");

/// Typed map of database-specific properties, ordered by id so output is deterministic.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Extras(BTreeMap<PropId, Value>);

impl Extras {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a property, returning the previous value.
    pub fn insert(&mut self, prop: PropId, value: impl Into<Value>) -> Option<Value> {
        self.0.insert(prop, value.into())
    }

    /// Builder form of [`Extras::insert`].
    #[must_use]
    pub fn with(mut self, prop: PropId, value: impl Into<Value>) -> Self {
        self.insert(prop, value);
        self
    }

    pub fn get(&self, prop: &PropId) -> Option<&Value> {
        self.0.get(prop)
    }

    pub fn remove(&mut self, prop: &PropId) -> Option<Value> {
        self.0.remove(prop)
    }

    pub fn contains(&self, prop: &PropId) -> bool {
        self.0.contains_key(prop)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&PropId, &Value)> {
        self.0.iter()
    }

    /// Ids present in either map whose values differ (including present-vs-absent), in id order.
    pub fn changed_keys<'a>(&'a self, other: &'a Extras) -> Vec<&'a PropId> {
        let mut keys: Vec<&PropId> = self
            .0
            .keys()
            .chain(other.0.keys())
            .filter(|k| self.0.get(*k) != other.0.get(*k))
            .collect();
        keys.sort();
        keys.dedup();
        keys
    }
}

impl FromIterator<(PropId, Value)> for Extras {
    fn from_iter<I: IntoIterator<Item = (PropId, Value)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl<'a> IntoIterator for &'a Extras {
    type Item = (&'a PropId, &'a Value);
    type IntoIter = std::collections::btree_map::Iter<'a, PropId, Value>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn static_and_owned_ids_compare_equal() {
        assert_eq!(PropId::new("tablespace"), TABLESPACE);
        assert_eq!(TABLESPACE.to_string(), "tablespace");
    }

    #[test]
    fn extras_map() {
        let a = Extras::new().with(UNLOGGED, true).with(TABLESPACE, "fast");
        let b = Extras::new()
            .with(TABLESPACE, "slow")
            .with(FILL_FACTOR, 90_i32);
        assert_eq!(a.get(&UNLOGGED), Some(&Value::Bool(true)));
        assert_eq!(a.len(), 2);
        let changed: Vec<&str> = a.changed_keys(&b).into_iter().map(PropId::as_str).collect();
        assert_eq!(changed, ["fill_factor", "tablespace", "unlogged"]);
        assert_eq!(a.changed_keys(&a).len(), 0);
    }

    #[test]
    fn serde_is_a_plain_object() {
        let e = Extras::new().with(WITHOUT_ROWID, true);
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(json, r#"{"without_rowid":{"type":"bool","value":true}}"#);
        assert_eq!(serde_json::from_str::<Extras>(&json).unwrap(), e);
    }
}
