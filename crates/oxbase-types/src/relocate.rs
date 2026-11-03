//! Moving object definitions between containers (M8, `docs/m8-contracts.md` §3.2).
//!
//! Structure sync, data transfer and restore read objects in one container (the *source*
//! database or schema) and write them into another (the *target*). A definition fetched from the
//! source names the source everywhere: its own [`ObjectRef`], the table of a child (column,
//! index, foreign key, check), the referenced table of a foreign key, the function a trigger
//! runs, the column that owns a sequence. Before a source definition is compared with, or
//! created in, the target, every such ref is moved with [`relocate_def`].
//!
//! # Matching
//!
//! A ref lives in a [`Location`] when its `schema` equals the location's `schema` (both `None`
//! counts as equal) and its `catalog` is `None` (the session's current database) or equals the
//! location's `catalog`. Refs elsewhere (a foreign key to another schema, a trigger function in
//! `public`) are left as they are: they point at objects outside the compared containers.
//!
//! Moving sets `schema` to the target's schema and, when the ref named its catalog, `catalog` to
//! the target's catalog (a ref without a catalog stays relative).
//!
//! Engine-native SQL text inside a definition (view queries, routine bodies, defaults, check
//! expressions) is **not** rewritten here: names inside SQL need the engine's lexical rules
//! (`oxbase_diff::retarget_sql`).

use serde::{Deserialize, Serialize};

use crate::{ObjectDef, ObjectKind, ObjectRef};

/// Where objects live: a database (`catalog`) and, on engines with schemas, a schema.
///
/// | Engine | Container ref | Location |
/// |---|---|---|
/// | PostgreSQL, SQL Server | `Schema` ref `{catalog: db, name: s}` | `{catalog: db, schema: s}` |
/// | MySQL, MariaDB | `Database` ref `{name: db}` | `{catalog: db, schema: None}` |
/// | SQLite | `Database` ref `{name: "main"}` (or an attached name) | `{catalog: "main", schema: None}` |
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Location {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
}

impl Location {
    pub fn new(catalog: Option<&str>, schema: Option<&str>) -> Self {
        Self {
            catalog: catalog.map(str::to_owned),
            schema: schema.map(str::to_owned),
        }
    }

    /// The location a container ref names: a `Database` ref is that catalog, a `Schema` ref
    /// that schema in its catalog; any other ref, the catalog and schema it lives in.
    pub fn of_container(container: &ObjectRef) -> Self {
        match container.kind {
            ObjectKind::Database => Self {
                catalog: (!container.name.is_empty()).then(|| container.name.clone()),
                schema: None,
            },
            ObjectKind::Schema => Self {
                catalog: container.catalog.clone(),
                schema: Some(container.name.clone()),
            },
            _ => Self::of(container),
        }
    }

    /// The catalog and schema `r` lives in.
    pub fn of(r: &ObjectRef) -> Self {
        Self {
            catalog: r.catalog.clone(),
            schema: r.schema.clone(),
        }
    }

    /// The container ref of this location: a `Schema` ref when it has a schema, else a
    /// `Database` ref (named after the catalog; the root when there is none).
    pub fn container_ref(&self) -> ObjectRef {
        match &self.schema {
            Some(schema) => ObjectRef::schema(self.catalog.as_deref(), schema.clone()),
            None => match &self.catalog {
                Some(catalog) => ObjectRef::database(catalog.clone()),
                None => ObjectRef::root(),
            },
        }
    }

    /// `true` when `r` lives here (module docs, *Matching*). Database and schema refs never do:
    /// they are containers, not contents.
    pub fn contains(&self, r: &ObjectRef) -> bool {
        if matches!(r.kind, ObjectKind::Database | ObjectKind::Schema) {
            return false;
        }
        r.schema == self.schema && (r.catalog.is_none() || r.catalog == self.catalog)
    }
}

/// Moves `r` from `from` to `to` when it lives in `from` (module docs). Returns whether it
/// moved.
pub fn relocate_ref(r: &mut ObjectRef, from: &Location, to: &Location) -> bool {
    if !from.contains(r) {
        return false;
    }
    r.schema.clone_from(&to.schema);
    if r.catalog.is_some() {
        r.catalog.clone_from(&to.catalog);
    }
    true
}

/// Moves every ref inside `def` that lives in `from` to `to`: the object itself, the owning
/// table of a child definition, foreign-key targets, a trigger's function and a sequence's
/// owning column. SQL text is left alone (module docs). `Schema` definitions (containers) are
/// not changed.
pub fn relocate_def(def: &mut ObjectDef, from: &Location, to: &Location) {
    match def {
        ObjectDef::Schema(_) => {}
        ObjectDef::Table(t) => {
            relocate_ref(&mut t.obj, from, to);
            for fk in &mut t.foreign_keys {
                relocate_ref(&mut fk.ref_table, from, to);
            }
        }
        ObjectDef::Column { table, .. }
        | ObjectDef::Index { table, .. }
        | ObjectDef::Check { table, .. } => {
            relocate_ref(table, from, to);
        }
        ObjectDef::ForeignKey { table, foreign_key } => {
            relocate_ref(table, from, to);
            relocate_ref(&mut foreign_key.ref_table, from, to);
        }
        ObjectDef::View(v) => {
            relocate_ref(&mut v.obj, from, to);
        }
        ObjectDef::Routine(r) => {
            relocate_ref(&mut r.obj, from, to);
        }
        ObjectDef::Trigger(t) => {
            relocate_ref(&mut t.obj, from, to);
            if let Some(function) = &mut t.function {
                relocate_ref(function, from, to);
            }
        }
        ObjectDef::Sequence(q) => {
            relocate_ref(&mut q.obj, from, to);
            if let Some(owner) = &mut q.owned_by {
                relocate_ref(owner, from, to);
            }
        }
        ObjectDef::Generic(g) => {
            relocate_ref(&mut g.obj, from, to);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ColumnDef, DataType, Extras, FkAction, ForeignKeyDef, SequenceDef, TableDef, TriggerDef,
        TriggerEvent, TriggerLevel, TriggerTiming,
    };
    use pretty_assertions::assert_eq;

    fn fk(to: ObjectRef) -> ForeignKeyDef {
        ForeignKeyDef {
            name: Some("fk".into()),
            columns: vec!["p".into()],
            ref_table: to,
            ref_columns: vec!["id".into()],
            on_delete: FkAction::NoAction,
            on_update: FkAction::NoAction,
            extras: Extras::default(),
        }
    }

    #[test]
    fn locations_of_containers() {
        let pg = ObjectRef::schema(Some("oxbase_test"), "m8_src");
        assert_eq!(
            Location::of_container(&pg),
            Location::new(Some("oxbase_test"), Some("m8_src"))
        );
        assert_eq!(Location::of_container(&pg).container_ref(), pg);
        let my = ObjectRef::database("m8_src");
        assert_eq!(
            Location::of_container(&my),
            Location::new(Some("m8_src"), None)
        );
        assert_eq!(Location::of_container(&my).container_ref(), my);
        assert_eq!(Location::default().container_ref(), ObjectRef::root());
        let table = ObjectRef::table(Some("db"), Some("s"), "t");
        assert_eq!(Location::of_container(&table), Location::of(&table));
    }

    #[test]
    fn moves_a_postgres_table_and_its_foreign_keys() {
        let from = Location::new(Some("oxbase_test"), Some("m8_src"));
        let to = Location::new(Some("oxbase_test"), Some("m8_tgt"));
        let mut t = TableDef::new(ObjectRef::table(
            Some("oxbase_test"),
            Some("m8_src"),
            "child",
        ));
        t.columns.push(ColumnDef::new("p", DataType::INT64));
        // Inside the source schema (with and without a catalog), and outside it.
        t.foreign_keys
            .push(fk(ObjectRef::table(None, Some("m8_src"), "parent")));
        t.foreign_keys.push(fk(ObjectRef::table(
            Some("oxbase_test"),
            Some("m8_src"),
            "other",
        )));
        t.foreign_keys
            .push(fk(ObjectRef::table(None, Some("public"), "shared")));
        let mut def = ObjectDef::Table(t);
        relocate_def(&mut def, &from, &to);
        let ObjectDef::Table(t) = def else {
            panic!("not a table")
        };
        assert_eq!(
            t.obj,
            ObjectRef::table(Some("oxbase_test"), Some("m8_tgt"), "child")
        );
        assert_eq!(
            t.foreign_keys[0].ref_table,
            ObjectRef::table(None, Some("m8_tgt"), "parent")
        );
        assert_eq!(
            t.foreign_keys[1].ref_table,
            ObjectRef::table(Some("oxbase_test"), Some("m8_tgt"), "other")
        );
        assert_eq!(
            t.foreign_keys[2].ref_table,
            ObjectRef::table(None, Some("public"), "shared")
        );
    }

    #[test]
    fn moves_mysql_children_triggers_and_sequences() {
        let from = Location::of_container(&ObjectRef::database("m8_src"));
        let to = Location::of_container(&ObjectRef::database("m8_tgt"));
        let table = ObjectRef::table(Some("m8_src"), None, "t");

        let mut col = ObjectDef::Column {
            table: table.clone(),
            column: ColumnDef::new("c", DataType::Text),
        };
        relocate_def(&mut col, &from, &to);
        assert_eq!(
            col.object_ref(),
            ObjectRef::table(Some("m8_tgt"), None, "t").child(ObjectKind::Column, "c")
        );

        let mut trg = ObjectDef::Trigger(TriggerDef {
            obj: table.child(ObjectKind::Trigger, "trg"),
            timing: TriggerTiming::After,
            events: vec![TriggerEvent::Insert],
            update_columns: vec![],
            level: TriggerLevel::Row,
            condition: None,
            function: Some(ObjectRef::new(ObjectKind::Function, "f").in_catalog(Some("other"))),
            body: Some("BEGIN END".into()),
            definition: None,
            enabled: true,
            comment: None,
            extras: Extras::default(),
        });
        relocate_def(&mut trg, &from, &to);
        let ObjectDef::Trigger(t) = &trg else {
            panic!("not a trigger")
        };
        assert_eq!(t.obj.catalog.as_deref(), Some("m8_tgt"));
        assert_eq!(t.obj.parent.as_deref(), Some("t"));
        // Another database: untouched.
        assert_eq!(
            t.function.as_ref().and_then(|f| f.catalog.as_deref()),
            Some("other")
        );

        let mut seq = SequenceDef::new(ObjectRef::new(ObjectKind::Sequence, "s"));
        seq.owned_by = Some(table.child(ObjectKind::Column, "id"));
        let mut seq = ObjectDef::Sequence(seq);
        relocate_def(&mut seq, &from, &to);
        let ObjectDef::Sequence(q) = &seq else {
            panic!("not a sequence")
        };
        // A ref without a catalog stays relative.
        assert_eq!(q.obj.catalog, None);
        assert_eq!(
            q.owned_by.as_ref().and_then(|o| o.catalog.as_deref()),
            Some("m8_tgt")
        );
    }

    #[test]
    fn containers_never_move() {
        let from = Location::new(Some("db"), None);
        let to = Location::new(Some("db2"), None);
        let mut db = ObjectRef::database("db");
        assert!(!relocate_ref(&mut db, &from, &to));
        assert_eq!(db, ObjectRef::database("db"));
        let json = serde_json::to_string(&Location::new(Some("db"), None)).unwrap();
        assert_eq!(json, r#"{"catalog":"db"}"#);
    }
}
