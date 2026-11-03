//! Structure compare (`docs/m8-contracts.md` §5.1-§5.3): two schemas of the **same engine**,
//! read through the engine's fetchers into [`SchemaSnapshot`]s and compared with its
//! `Comparator` cells into a [`StructureDiff`]. Owner of the bodies marked
//! `unimplemented!("m8: structure-sync")`: the **structure-sync** stream.
//!
//! # Snapshots ([`load_snapshot`])
//!
//! Every object of [`StructureOptions::kinds`] under the container (a `Schema` ref on
//! PostgreSQL / SQL Server, a `Database` ref on MySQL / MariaDB / SQLite): `list` then `load`
//! through the engine's `Fetcher`s (system objects skipped). Tables are loaded whole (columns,
//! keys, indexes, foreign keys, checks); triggers are listed per table and view. An object
//! that fails to load (permissions, an unsupported feature) is skipped with a reason
//! ([`SchemaSnapshot::skipped`]) instead of failing the compare. Kinds the engine has no
//! fetcher for, or does not have at its version (`Capabilities::supports_kind`), are skipped
//! silently.
//!
//! # Matching and comparing ([`compare`])
//!
//! 1. Every source definition is moved to the target container
//!    (`oxbase_types::relocate_def`) and the names inside its SQL text are moved with
//!    [`crate::retarget_sql`], so a view `SELECT ... FROM m8_src.t` compares equal to
//!    `SELECT ... FROM m8_tgt.t`.
//! 2. Objects are matched by kind and name (and parent for triggers, signature for
//!    PostgreSQL routines); exactly, or ignoring case with [`StructureOptions::ignore_case`]
//!    (the target's spelling is kept). Excluded objects ([`StructureOptions::exclude`], by
//!    source or target ref) are left out of the diff.
//! 3. Matched pairs go through the engine's `Comparator` for their kind (`diff(target,
//!    source)`: going **from the target to the source**, because the script makes the target
//!    look like the source). Property differences the options switch off are removed: collations
//!    (`collation`), comments (`comment`), ownership (`owner`, `definer`).
//! 4. Every item gets both sides' `CREATE` text for the side-by-side DDL view (the engine's
//!    `DdlGenerator::create`, rendered with the engine's terminator), so the UI never builds
//!    DDL itself.
//!
//! # Determinism
//!
//! Items are sorted by kind rank (table, view, materialized view, sequence, function,
//! procedure, trigger), then name, then parent; [`DiffItem::id`] is stable across compares of
//! the same schemas (`<kind>:<parent>.<name>(<signature>)`, target-side names), so a selection
//! survives a re-compare.

use oxbase_dialect::DialectRegistry;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{Engine, ObjectDef, ObjectKind, ObjectRef, PropertyDiff, Result, ServerVersion};
use serde::{Deserialize, Serialize};

/// The top-level kinds a structure compare reads by default (their children come with them:
/// a table's columns, keys, indexes, foreign keys and checks; triggers per table).
pub const DEFAULT_KINDS: [ObjectKind; 7] = [
    ObjectKind::Table,
    ObjectKind::View,
    ObjectKind::MatView,
    ObjectKind::Sequence,
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Trigger,
];

/// What a compare reads and how it compares. Every field has a default (camelCase JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StructureOptions {
    /// Top-level kinds to compare (default [`DEFAULT_KINDS`]).
    pub kinds: Vec<ObjectKind>,
    /// Scripts drop objects that exist only in the target (off by default; drops are
    /// destructive and need confirmation to apply).
    pub include_drops: bool,
    pub compare_collations: bool,
    pub compare_comments: bool,
    /// Owners, definers and security principals (off by default: two servers rarely share
    /// them).
    pub compare_ownership: bool,
    /// Match names ignoring case (for engines or schemas whose names were created with a
    /// different case).
    pub ignore_case: bool,
    /// Objects to leave out (source- or target-side refs; a table excludes its triggers too).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<ObjectRef>,
}

impl Default for StructureOptions {
    fn default() -> Self {
        Self {
            kinds: DEFAULT_KINDS.to_vec(),
            include_drops: false,
            compare_collations: true,
            compare_comments: true,
            compare_ownership: false,
            ignore_case: false,
            exclude: Vec::new(),
        }
    }
}

/// An object a snapshot could not load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedObject {
    pub object: ObjectRef,
    pub reason: String,
}

/// Every compared object of one container, as the server reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaSnapshot {
    pub engine: Engine,
    pub version: ServerVersion,
    /// The container (`Schema` or `Database` ref).
    pub container: ObjectRef,
    /// Top-level definitions (tables whole; triggers separately), in fetcher order.
    pub objects: Vec<ObjectDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedObject>,
}

/// Reads `container` on `connection` (module docs, *Snapshots*). `on_object` is called after
/// each loaded object with (loaded so far, listed so far) for progress.
pub async fn load_snapshot(
    dialects: &DialectRegistry,
    connection: &dyn Connection,
    container: &ObjectRef,
    options: &StructureOptions,
    on_object: &(dyn Fn(u64, u64) + Send + Sync),
    cancel: CancellationToken,
) -> Result<SchemaSnapshot> {
    let _ = (dialects, connection, container, options, on_object, cancel);
    // Keeps the skeleton an `async fn` (clippy::unused_async) until the body lands.
    std::future::ready(()).await;
    unimplemented!("m8: structure-sync")
}

/// Where an object is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStatus {
    /// Only in the source: the script creates it.
    OnlyInSource,
    /// Only in the target: the script drops it when drops are included.
    OnlyInTarget,
    /// In both, with differences: the script alters (or re-creates) it.
    Different,
    /// In both, equal.
    Same,
}

/// One compared object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffItem {
    /// Stable id (module docs, *Determinism*); what selections name.
    pub id: String,
    pub kind: ObjectKind,
    /// Display name (target side when both exist).
    pub name: String,
    /// The owning table or view of a trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub status: DiffStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ObjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ObjectRef>,
    /// What differs (status `different`), going from the target to the source, in the
    /// comparator's fixed order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub properties: Vec<PropertyDiff>,
    /// The source object's `CREATE` text (relocated to the target container).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ddl: Option<String>,
    /// The target object's `CREATE` text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_ddl: Option<String>,
}

/// Item counts by status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffCounts {
    pub only_in_source: u32,
    pub only_in_target: u32,
    pub different: u32,
    pub same: u32,
}

/// The result of [`compare`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructureDiff {
    pub engine: Engine,
    pub source: ObjectRef,
    pub target: ObjectRef,
    /// Sorted (module docs, *Determinism*); `same` items included so the tree can show them.
    pub items: Vec<DiffItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl StructureDiff {
    /// Counts by status.
    pub fn counts(&self) -> DiffCounts {
        let mut c = DiffCounts::default();
        for item in &self.items {
            match item.status {
                DiffStatus::OnlyInSource => c.only_in_source += 1,
                DiffStatus::OnlyInTarget => c.only_in_target += 1,
                DiffStatus::Different => c.different += 1,
                DiffStatus::Same => c.same += 1,
            }
        }
        c
    }

    /// `true` when nothing differs: every item is `same`, or only in the target while drops
    /// are not compared (`include_drops` off). This is the after-apply re-compare check.
    pub fn is_in_sync(&self, include_drops: bool) -> bool {
        self.items.iter().all(|i| match i.status {
            DiffStatus::Same => true,
            DiffStatus::OnlyInTarget => !include_drops,
            DiffStatus::OnlyInSource | DiffStatus::Different => false,
        })
    }

    /// The item with `id`.
    pub fn item(&self, id: &str) -> Option<&DiffItem> {
        self.items.iter().find(|i| i.id == id)
    }
}

/// Compares two snapshots of the same engine (module docs, *Matching and comparing*).
/// `InvalidInput` when the engines differ.
pub fn compare(
    dialects: &DialectRegistry,
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
    options: &StructureOptions,
) -> Result<StructureDiff> {
    let _ = (dialects, source, target, options);
    unimplemented!("m8: structure-sync")
}

/// The stable id of an object (module docs, *Determinism*): `<kind>:<parent>.<name>` with
/// `(<signature>)` for routines that have one; names unquoted.
pub fn item_id(r: &ObjectRef) -> String {
    let mut id = format!("{}:", r.kind);
    if let Some(parent) = &r.parent {
        id.push_str(parent);
        id.push('.');
    }
    id.push_str(&r.name);
    if let Some(signature) = &r.signature {
        id.push('(');
        id.push_str(signature);
        id.push(')');
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn item(id: &str, status: DiffStatus) -> DiffItem {
        DiffItem {
            id: id.into(),
            kind: ObjectKind::Table,
            name: id.into(),
            parent: None,
            status,
            source: None,
            target: None,
            properties: Vec::new(),
            source_ddl: None,
            target_ddl: None,
        }
    }

    #[test]
    fn ids_counts_and_sync_state() {
        let t = ObjectRef::table(Some("db"), Some("s"), "orders");
        assert_eq!(item_id(&t), "table:orders");
        assert_eq!(
            item_id(&t.child(ObjectKind::Trigger, "audit")),
            "trigger:orders.audit"
        );
        let f = ObjectRef::schema(Some("db"), "s")
            .child(ObjectKind::Function, "f")
            .with_signature("integer");
        assert_eq!(item_id(&f), "function:f(integer)");

        let mut diff = StructureDiff {
            engine: Engine::Postgres,
            source: ObjectRef::schema(Some("db"), "src"),
            target: ObjectRef::schema(Some("db"), "tgt"),
            items: vec![
                item("table:a", DiffStatus::Same),
                item("table:b", DiffStatus::OnlyInTarget),
            ],
            warnings: Vec::new(),
        };
        assert!(diff.is_in_sync(false));
        assert!(!diff.is_in_sync(true));
        diff.items.push(item("table:c", DiffStatus::Different));
        assert!(!diff.is_in_sync(false));
        assert_eq!(
            diff.counts(),
            DiffCounts {
                only_in_source: 0,
                only_in_target: 1,
                different: 1,
                same: 1
            }
        );
        assert!(diff.item("table:c").is_some());
        let json = serde_json::to_value(&diff).unwrap();
        assert_eq!(json["items"][1]["status"], "only_in_target");
        assert_eq!(serde_json::from_value::<StructureDiff>(json).unwrap(), diff);
    }

    #[test]
    fn options_default_to_every_kind_without_drops() {
        let o: StructureOptions = serde_json::from_str("{}").unwrap();
        assert_eq!(o, StructureOptions::default());
        assert_eq!(o.kinds.len(), 7);
        assert!(!o.include_drops && o.compare_comments && !o.compare_ownership);
        let json = serde_json::to_value(&o).unwrap();
        assert_eq!(json["includeDrops"], false);
        assert!(json.get("exclude").is_none());
    }
}
