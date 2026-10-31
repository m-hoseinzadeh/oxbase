//! Object designer (M3): the table designer's editable model ([`TableDesign`]), its validation
//! ([`validate`]) and its DDL preview ([`preview`]) for one engine. Pure: no I/O, no sessions.
//! `oxbase_services::designer::DesignerService` loads tables, calls these functions and applies
//! the script; the UI edits a [`TableDesign`] and sends it back whole.
//!
//! # Flow
//!
//! 1. **Load**: the engine's `Table` fetcher returns a [`TableDef`]; [`TableDesign::from_def`]
//!    wraps it (every column, index, foreign key and check remembers its `origin`, its index in
//!    the loaded definition). A new table starts from [`TableDesign::new_table`].
//! 2. **Edit** (UI): change, add, remove and reorder items. Items keep their `origin`; new ones
//!    have none. A literal default typed by the user goes in [`DesignColumn::default_literal`]
//!    (the dialect quotes it); `column.default` is an engine-native expression.
//! 3. **Validate** ([`validate`]): names, references, expressions
//!    (`oxbase_dialect::sql::check_expression`) and what the engine's
//!    [`DesignerCapabilities`] allow. Errors block the preview's script and the apply; warnings
//!    are shown.
//! 4. **Diff**: [`TableDesign::renames`] (from origins), [`TableDesign::to_def`] (the new
//!    definition), then the engine's `Comparator` between the original with the renames applied
//!    ([`apply_renames`]) and the new definition, prefixed with one `name` difference per
//!    rename.
//! 5. **DDL**: `DdlGenerator::create` for a new table, else `DdlGenerator::alter(original, new)`
//!    with a `DdlContext` carrying the original table, its triggers and the renames.
//! 6. **Preview** ([`DesignPreview`]): the differences, the script, its SQL text
//!    ([`render_script`]), whether it is atomic (the engine has transactional DDL and every main
//!    step is transactional), whether it is destructive, the issues and the engine's notes.
//!    When it is not atomic the notes say that a failure leaves the earlier statements applied
//!    (MySQL/MariaDB).
//! 7. **Apply** and **reload**: in services (see `DesignerService::apply`).
//!
//! Owner of the bodies marked `unimplemented!("m3: designer")`: the **designer** stream
//! (`docs/m3-contracts.md`).
//!
//! Serde: the wrapper types are camelCase (UI payloads); the core models inside them
//! (`TableDef`, `ColumnDef`, `IndexDef`, ...) keep their snake_case form.

use oxbase_dialect::{Comparator, DdlGenerator, DesignerCapabilities, Quoting};
use oxbase_types::{
    CheckDef, ColumnDef, Extras, ForeignKeyDef, IndexDef, ObjectKind, ObjectRef, PrimaryKeyDef,
    PropertyDiff, Rename, Result, Script, ServerVersion, TableDef, TriggerDef,
};
use serde::{Deserialize, Serialize};

/// The table designer's editable model. See the crate docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableDesign {
    /// The table as loaded; `None` when designing a new table. Sent back unchanged by the UI:
    /// the apply compares it with the table on the server and refuses when they differ (someone
    /// else changed the table since it was loaded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<TableDef>,
    /// Where the table lives and its (new) name: a `Table` ref with the catalog and schema of
    /// the original, or of the schema / database a new table goes into.
    pub table: ObjectRef,
    /// Columns in their new order.
    pub columns: Vec<DesignColumn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_key: Option<PrimaryKeyDef>,
    #[serde(default)]
    pub indexes: Vec<DesignItem<IndexDef>>,
    #[serde(default)]
    pub foreign_keys: Vec<DesignItem<ForeignKeyDef>>,
    #[serde(default)]
    pub checks: Vec<DesignItem<CheckDef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Engine-specific table properties (gated by `Capabilities::supports`).
    #[serde(default)]
    pub extras: Extras,
}

/// One column of a [`TableDesign`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesignColumn {
    /// Index of this column in `original.columns`; `None` for a new column. Two columns never
    /// share an origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<usize>,
    pub column: ColumnDef,
    /// A default typed as a plain value: replaces `column.default` with this text as a string
    /// literal quoted by the dialect (`Quoting::quote_str`), so quotes and backslashes cannot
    /// change the statement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_literal: Option<String>,
}

/// An index, foreign key or check of a [`TableDesign`], with its origin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesignItem<T> {
    /// Index in the original's list of this kind; `None` when new.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<usize>,
    pub def: T,
}

impl<T> DesignItem<T> {
    pub fn new(def: T) -> Self {
        Self { origin: None, def }
    }
}

fn with_origins<T: Clone>(items: &[T]) -> Vec<DesignItem<T>> {
    items
        .iter()
        .enumerate()
        .map(|(idx, def)| DesignItem {
            origin: Some(idx),
            def: def.clone(),
        })
        .collect()
}

impl TableDesign {
    /// An empty design for a new table `table` (a `Table` ref in its schema / database).
    pub fn new_table(table: ObjectRef) -> Self {
        Self {
            original: None,
            table,
            columns: Vec::new(),
            primary_key: None,
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            checks: Vec::new(),
            comment: None,
            extras: Extras::new(),
        }
    }

    /// A design of the loaded table `def`, every item pointing at its origin.
    pub fn from_def(def: TableDef) -> Self {
        Self {
            table: def.obj.clone(),
            columns: def
                .columns
                .iter()
                .enumerate()
                .map(|(idx, column)| DesignColumn {
                    origin: Some(idx),
                    column: column.clone(),
                    default_literal: None,
                })
                .collect(),
            primary_key: def.primary_key.clone(),
            indexes: with_origins(&def.indexes),
            foreign_keys: with_origins(&def.foreign_keys),
            checks: with_origins(&def.checks),
            comment: def.comment.clone(),
            extras: def.extras.clone(),
            original: Some(def),
        }
    }

    /// `true` for a table that does not exist yet.
    pub fn is_new(&self) -> bool {
        self.original.is_none()
    }

    /// The renames implied by the origins: the table (when its name changed), then columns,
    /// indexes, foreign keys and checks whose name differs from their origin's (constraints only
    /// when both names are set). Empty for a new table.
    pub fn renames(&self) -> Vec<Rename> {
        let Some(original) = &self.original else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if original.obj.name != self.table.name {
            out.push(Rename::new(
                ObjectKind::Table,
                &original.obj.name,
                &self.table.name,
            ));
        }
        for col in &self.columns {
            if let Some(old) = col.origin.and_then(|idx| original.columns.get(idx))
                && old.name != col.column.name
            {
                out.push(Rename::new(ObjectKind::Column, &old.name, &col.column.name));
            }
        }
        for idx in &self.indexes {
            if let Some(old) = idx.origin.and_then(|pos| original.indexes.get(pos))
                && old.name != idx.def.name
            {
                out.push(Rename::new(ObjectKind::Index, &old.name, &idx.def.name));
            }
        }
        for fk in &self.foreign_keys {
            if let Some(old) = fk.origin.and_then(|pos| original.foreign_keys.get(pos))
                && let (Some(before), Some(after)) = (&old.name, &fk.def.name)
                && before != after
            {
                out.push(Rename::new(ObjectKind::ForeignKey, before, after));
            }
        }
        for check in &self.checks {
            if let Some(old) = check.origin.and_then(|pos| original.checks.get(pos))
                && let (Some(before), Some(after)) = (&old.name, &check.def.name)
                && before != after
            {
                out.push(Rename::new(ObjectKind::Check, before, after));
            }
        }
        out
    }

    /// The new definition: the design's items in order, with each
    /// [`DesignColumn::default_literal`] turned into `column.default` by `quoting`.
    pub fn to_def(&self, quoting: &dyn Quoting) -> TableDef {
        let mut def = TableDef::new(self.table.clone());
        def.columns = self
            .columns
            .iter()
            .map(|col| {
                let mut column = col.column.clone();
                if let Some(literal) = &col.default_literal {
                    column.default = Some(quoting.quote_str(literal));
                }
                column
            })
            .collect();
        def.primary_key.clone_from(&self.primary_key);
        def.indexes = self.indexes.iter().map(|item| item.def.clone()).collect();
        def.foreign_keys = self
            .foreign_keys
            .iter()
            .map(|item| item.def.clone())
            .collect();
        def.checks = self.checks.iter().map(|item| item.def.clone()).collect();
        def.comment.clone_from(&self.comment);
        def.extras = self.extras.clone();
        def
    }
}

/// `def` with `renames` applied: the table name; column names everywhere they appear (columns,
/// primary key, plain index keys, foreign-key columns); index, foreign-key and check names.
/// Expressions are not rewritten. Used to compare the original with the new definition so a
/// rename is not reported as a drop plus an add.
pub fn apply_renames(def: &TableDef, renames: &[Rename]) -> TableDef {
    let mut out = def.clone();
    let renamed = |kind: ObjectKind, name: &str| -> Option<String> {
        renames
            .iter()
            .find(|r| r.kind == kind && r.old == name)
            .map(|r| r.new.clone())
    };
    let column = |name: &mut String| {
        if let Some(new) = renamed(ObjectKind::Column, name) {
            *name = new;
        }
    };
    if let Some(new) = renamed(ObjectKind::Table, &out.obj.name) {
        out.obj.name = new;
    }
    for col in &mut out.columns {
        column(&mut col.name);
    }
    if let Some(pk) = &mut out.primary_key {
        pk.columns.iter_mut().for_each(column);
    }
    for idx in &mut out.indexes {
        if let Some(new) = renamed(ObjectKind::Index, &idx.name) {
            idx.name = new;
        }
        for part in idx.columns.iter_mut().filter(|part| !part.is_expression) {
            column(&mut part.name);
        }
    }
    for fk in &mut out.foreign_keys {
        if let Some(new) = fk
            .name
            .as_deref()
            .and_then(|n| renamed(ObjectKind::ForeignKey, n))
        {
            fk.name = Some(new);
        }
        fk.columns.iter_mut().for_each(column);
    }
    for check in &mut out.checks {
        if let Some(new) = check
            .name
            .as_deref()
            .and_then(|n| renamed(ObjectKind::Check, n))
        {
            check.name = Some(new);
        }
    }
    out
}

/// How bad a [`DesignIssue`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IssueSeverity {
    /// Blocks the script and the apply.
    Error,
    /// Shown; does not block.
    Warning,
}

/// A problem [`validate`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesignIssue {
    pub severity: IssueSeverity,
    /// Where, as a path into the design: `table`, `columns[2].name`, `indexes[0].columns`,
    /// `foreignKeys[1]`, `checks[0].expression`, `primaryKey`, ...
    pub path: String,
    pub message: String,
}

impl DesignIssue {
    pub fn error(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: IssueSeverity::Error,
            path: path.into(),
            message: message.into(),
        }
    }

    pub fn warning(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: IssueSeverity::Warning,
            path: path.into(),
            message: message.into(),
        }
    }
}

/// What a preview needs from the engine (resolved by the designer service from the dialect
/// registry and the session).
#[derive(Clone, Copy)]
pub struct PreviewInput<'a> {
    /// The engine's `Table` generator.
    pub ddl: &'a dyn DdlGenerator,
    /// The engine's `Table` comparator.
    pub comparator: &'a dyn Comparator,
    pub quoting: &'a dyn Quoting,
    pub capabilities: &'a DesignerCapabilities,
    pub version: &'a ServerVersion,
    /// Triggers on the table (for engines whose rebuild re-creates them).
    pub triggers: &'a [TriggerDef],
    /// Statement terminator of the preview text (`;`).
    pub terminator: &'a str,
}

/// The designer's preview of a design. See the crate docs (step 6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesignPreview {
    /// What changes, in the comparator's order (renames first). A new table: one `added`
    /// difference for the table.
    pub diffs: Vec<PropertyDiff>,
    /// The DDL; empty when nothing changed or when an issue is an error.
    pub script: Script,
    /// The script as the preview shows it ([`render_script`]).
    pub sql: String,
    /// All-or-nothing: transactional DDL and every main step transactional.
    pub atomic: bool,
    /// Some step can lose data; the apply needs an explicit confirmation.
    pub destructive: bool,
    pub issues: Vec<DesignIssue>,
    /// Engine notes (capability notes, "not atomic: a failure leaves earlier statements
    /// applied", script warnings).
    pub notes: Vec<String>,
}

impl DesignPreview {
    /// `true` when an issue is an error (nothing may be applied).
    pub fn has_errors(&self) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.severity == IssueSeverity::Error)
    }
}

/// Checks a design against the engine (see the crate docs, step 3, and
/// `docs/m3-contracts.md` for the full list). Issues are in design order.
pub fn validate(design: &TableDesign, input: &PreviewInput<'_>) -> Vec<DesignIssue> {
    let _ = (design, input);
    unimplemented!("m3: designer")
}

/// Builds the preview of `design` (crate docs, steps 3-6). Fails only when the generator does
/// (`Unsupported` / `InvalidInput`); validation problems are issues in the preview.
pub fn preview(design: &TableDesign, input: &PreviewInput<'_>) -> Result<DesignPreview> {
    let _ = (design, input);
    unimplemented!("m3: designer")
}

/// The preview text of `script`: steps in execution order, each preceded by a `-- description`
/// comment line, prepare and finally steps marked as running outside the transaction, and
/// `BEGIN` / `COMMIT` lines around the main steps when `atomic`. Each statement ends with
/// `terminator` and a newline.
pub fn render_script(script: &Script, atomic: bool, terminator: &str) -> String {
    let _ = (script, atomic, terminator);
    unimplemented!("m3: designer")
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_dialect::{BacktickQuoting, PgQuoting};
    use oxbase_types::{DataType, FkAction, IndexColumn};
    use pretty_assertions::assert_eq;

    fn loaded() -> TableDef {
        let mut table = TableDef::new(ObjectRef::table(Some("db"), Some("public"), "orders"));
        table
            .columns
            .push(ColumnDef::new("id", DataType::INT64).not_null());
        table
            .columns
            .push(ColumnDef::new("customer", DataType::INT32));
        table.primary_key = Some(PrimaryKeyDef {
            name: Some("orders_pkey".into()),
            columns: vec!["id".into()],
        });
        table.indexes.push(IndexDef {
            name: "orders_customer_idx".into(),
            columns: vec![IndexColumn::column("customer")],
            unique: false,
            is_constraint: false,
            predicate: None,
            extras: Extras::new(),
        });
        table.foreign_keys.push(ForeignKeyDef {
            name: Some("orders_customer_fk".into()),
            columns: vec!["customer".into()],
            ref_table: ObjectRef::table(None, Some("public"), "customers"),
            ref_columns: vec!["id".into()],
            on_delete: FkAction::Cascade,
            on_update: FkAction::NoAction,
            extras: Extras::new(),
        });
        table.checks.push(CheckDef {
            name: None,
            expression: "customer > 0".into(),
            extras: Extras::new(),
        });
        table
    }

    #[test]
    fn loaded_design_round_trips_to_the_same_definition() {
        let def = loaded();
        let design = TableDesign::from_def(def.clone());
        assert!(!design.is_new());
        assert_eq!(design.columns[1].origin, Some(1));
        assert_eq!(design.indexes[0].origin, Some(0));
        assert_eq!(design.renames(), []);
        assert_eq!(design.to_def(&PgQuoting), def);
        assert_eq!(apply_renames(&def, &[]), def);
        let json = serde_json::to_value(&design).unwrap();
        assert!(json.get("primaryKey").is_some());
        assert_eq!(json["foreignKeys"][0]["origin"], 0);
        assert_eq!(json["columns"][0]["column"]["native_type"], "");
        assert_eq!(serde_json::from_value::<TableDesign>(json).unwrap(), design);
    }

    #[test]
    fn renames_follow_origins() {
        let def = loaded();
        let mut design = TableDesign::from_def(def.clone());
        design.table.name = "orders_v2".into();
        design.columns[1].column.name = "customer_id".into();
        design.indexes[0].def.name = "orders_customer_id_idx".into();
        design.foreign_keys[0].def.name = Some("orders_customer_id_fk".into());
        // A new column with an old name is not a rename.
        design.columns.push(DesignColumn {
            origin: None,
            column: ColumnDef::new("customer", DataType::Text),
            default_literal: None,
        });
        let renames = design.renames();
        assert_eq!(
            renames,
            [
                Rename::new(ObjectKind::Table, "orders", "orders_v2"),
                Rename::new(ObjectKind::Column, "customer", "customer_id"),
                Rename::new(
                    ObjectKind::Index,
                    "orders_customer_idx",
                    "orders_customer_id_idx"
                ),
                Rename::new(
                    ObjectKind::ForeignKey,
                    "orders_customer_fk",
                    "orders_customer_id_fk"
                ),
            ]
        );
        let renamed = apply_renames(&def, &renames);
        assert_eq!(renamed.obj.name, "orders_v2");
        assert_eq!(renamed.columns[1].name, "customer_id");
        assert_eq!(renamed.indexes[0].name, "orders_customer_id_idx");
        assert_eq!(renamed.indexes[0].columns[0].name, "customer_id");
        assert_eq!(renamed.foreign_keys[0].columns, ["customer_id"]);
        assert_eq!(
            renamed.foreign_keys[0].name.as_deref(),
            Some("orders_customer_id_fk")
        );
        // The PK did not reference the renamed column.
        assert_eq!(renamed.primary_key.as_ref().unwrap().columns, ["id"]);
    }

    #[test]
    fn literal_defaults_are_quoted_by_the_dialect() {
        let mut design = TableDesign::new_table(ObjectRef::table(Some("db"), None, "notes"));
        assert!(design.is_new());
        assert_eq!(design.renames(), []);
        design.columns.push(DesignColumn {
            origin: None,
            column: ColumnDef::new("body", DataType::Text),
            default_literal: Some("it's a \\ path".into()),
        });
        let mut expr = ColumnDef::new("at", DataType::Timestamp { precision: None });
        expr.default = Some("CURRENT_TIMESTAMP".into());
        design.columns.push(DesignColumn {
            origin: None,
            column: expr,
            default_literal: None,
        });
        let pg = design.to_def(&PgQuoting);
        assert_eq!(pg.columns[0].default.as_deref(), Some("'it''s a \\ path'"));
        assert_eq!(pg.columns[1].default.as_deref(), Some("CURRENT_TIMESTAMP"));
        let my = design.to_def(&BacktickQuoting);
        assert_eq!(
            my.columns[0].default.as_deref(),
            Some("'it''s a \\\\ path'")
        );
    }

    #[test]
    fn issues_and_preview_serde() {
        let issue = DesignIssue::error("columns[0].name", "empty name");
        let json = serde_json::to_value(&issue).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"severity": "error", "path": "columns[0].name",
                               "message": "empty name"})
        );
        let preview = DesignPreview {
            diffs: vec![],
            script: Script::single("ALTER TABLE t ADD c int"),
            sql: "ALTER TABLE t ADD c int;\n".into(),
            atomic: true,
            destructive: false,
            issues: vec![DesignIssue::warning("checks[0]", "not enforced")],
            notes: vec![],
        };
        assert!(!preview.has_errors());
        let json = serde_json::to_value(&preview).unwrap();
        assert_eq!(json["script"]["steps"][0]["sql"], "ALTER TABLE t ADD c int");
        assert_eq!(
            serde_json::from_value::<DesignPreview>(json).unwrap(),
            preview
        );
    }
}
