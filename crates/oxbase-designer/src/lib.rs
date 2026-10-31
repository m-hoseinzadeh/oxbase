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

use oxbase_dialect::sql::check_expression;
use oxbase_dialect::{
    Comparator, DdlContext, DdlGenerator, DesignerCapabilities, Quoting, Support,
};
use oxbase_types::{
    CheckDef, ColumnDef, DataType, Error, Extras, FkAction, ForeignKeyDef, IndexDef, ObjectDef,
    ObjectKind, ObjectRef, PrimaryKeyDef, PropertyDiff, Rename, Result, Script, ServerVersion,
    StepCheck, StepPhase, TableDef, TriggerDef, Value, props,
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
    let mut v = Validator {
        design,
        input,
        caps: input.capabilities,
        issues: Vec::new(),
        rebuild: false,
    };
    v.table();
    v.columns();
    v.primary_key();
    v.indexes();
    v.foreign_keys();
    v.checks();
    if v.rebuild {
        v.issues.push(DesignIssue::warning(
            "table",
            "this change rebuilds the table: its rows are copied into a new table",
        ));
    }
    v.issues
}

/// Builds the preview of `design` (crate docs, steps 3-6). Fails only when the generator does
/// (`Unsupported` / `InvalidInput`); validation problems are issues in the preview.
pub fn preview(design: &TableDesign, input: &PreviewInput<'_>) -> Result<DesignPreview> {
    let issues = validate(design, input);
    let new_def = design.to_def(input.quoting);
    let renames = design.renames();
    let diffs = match &design.original {
        None => vec![PropertyDiff::object_added(design.table.clone())],
        Some(original) => {
            let mut diffs: Vec<PropertyDiff> = renames
                .iter()
                .map(|rename| {
                    let object = if rename.kind == ObjectKind::Table {
                        new_def.obj.clone()
                    } else {
                        new_def.obj.child(rename.kind, rename.new.clone())
                    };
                    PropertyDiff::property(
                        object,
                        props::NAME,
                        Some(Value::Text(rename.old.clone())),
                        Some(Value::Text(rename.new.clone())),
                    )
                })
                .collect();
            diffs.extend(input.comparator.diff(
                &ObjectDef::Table(apply_renames(original, &renames)),
                &ObjectDef::Table(new_def.clone()),
            ));
            diffs
        }
    };
    let mut preview = DesignPreview {
        diffs,
        script: Script::new(),
        sql: String::new(),
        atomic: false,
        destructive: false,
        issues,
        notes: input.capabilities.notes.clone(),
    };
    if preview.has_errors() {
        return Ok(preview);
    }
    let base = DdlContext::new(input.version)
        .with_triggers(input.triggers)
        .with_renames(&renames);
    let script = match &design.original {
        None => input.ddl.create(&ObjectDef::Table(new_def), &base)?,
        Some(_) if preview.diffs.is_empty() => Script::new(),
        Some(original) => input.ddl.alter(
            &ObjectDef::Table(original.clone()),
            &ObjectDef::Table(new_def),
            &base.with_table(original),
        )?,
    };
    preview.atomic = input.capabilities.transactional_ddl && script.is_atomic();
    preview.destructive = script.is_destructive();
    if !script.is_empty() && !preview.atomic {
        preview.notes.push(NOT_ATOMIC_NOTE.to_owned());
    }
    preview.notes.extend(script.warnings.iter().cloned());
    preview.sql = render_script(&script, preview.atomic, input.terminator);
    preview.script = script;
    Ok(preview)
}

/// The note a preview carries when its script is not all-or-nothing.
pub const NOT_ATOMIC_NOTE: &str = "Not atomic: each statement is committed on its own, so if one \
    fails, the statements before it stay applied.";

/// The preview text of `script`: steps in execution order, each preceded by a `-- description`
/// comment line, prepare and finally steps marked as running outside the transaction, and
/// `BEGIN` / `COMMIT` lines around the main steps when `atomic`. Each statement ends with
/// `terminator` and a newline.
pub fn render_script(script: &Script, atomic: bool, terminator: &str) -> String {
    let mut out = String::new();
    let mut in_main = false;
    for step in script.ordered() {
        if step.phase == StepPhase::Main && !in_main {
            in_main = true;
            if atomic {
                out.push_str("BEGIN");
                out.push_str(terminator);
                out.push('\n');
            }
        }
        if step.phase == StepPhase::Finally && in_main {
            in_main = false;
            if atomic {
                out.push_str("COMMIT");
                out.push_str(terminator);
                out.push('\n');
            }
        }
        let mut notes: Vec<String> = Vec::new();
        match step.phase {
            StepPhase::Prepare => notes.push("before the transaction".to_owned()),
            StepPhase::Finally => notes.push("after the transaction, always".to_owned()),
            StepPhase::Main => {}
        }
        if let Some(condition) = &step.only_if {
            notes.push(format!(
                "only if {} = '{}'",
                condition.capture, condition.equals
            ));
        }
        if step.check == StepCheck::ExpectNoRows {
            notes.push("must return no rows".to_owned());
        }
        if step.destructive {
            notes.push("can lose data".to_owned());
        }
        let description = one_line(&step.description);
        let comment = match (description.is_empty(), notes.is_empty()) {
            (true, true) => None,
            (false, true) => Some(description),
            (true, false) => Some(notes.join("; ")),
            (false, false) => Some(format!("{description} ({})", notes.join("; "))),
        };
        if let Some(comment) = comment {
            out.push_str("-- ");
            out.push_str(&comment);
            out.push('\n');
        }
        out.push_str(&step.sql);
        out.push_str(terminator);
        out.push('\n');
    }
    if in_main && atomic {
        out.push_str("COMMIT");
        out.push_str(terminator);
        out.push('\n');
    }
    out
}

/// Text with line breaks turned into spaces (comment lines stay one line).
fn one_line(text: &str) -> String {
    text.split(['\r', '\n'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether values of the two types can reference each other (a foreign key and its target):
/// the same family (integers, exact and binary numbers, text, binary, dates, times,
/// timestamps, booleans, UUIDs, JSON). Engine-specific types (`Other`) and enums are not
/// judged.
pub fn compatible_types(a: &DataType, b: &DataType) -> bool {
    match (type_family(a), type_family(b)) {
        (None, _) | (_, None) => true,
        (Some(x), Some(y)) => x == y,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Number,
    Text,
    Binary,
    Date,
    Time,
    Timestamp,
    Bool,
    Uuid,
    Json,
    Interval,
}

fn type_family(t: &DataType) -> Option<Family> {
    Some(match t {
        DataType::Int { .. } | DataType::Decimal { .. } | DataType::Float { .. } => Family::Number,
        DataType::Char { .. } | DataType::VarChar { .. } | DataType::Text => Family::Text,
        DataType::Binary { .. } | DataType::VarBinary { .. } => Family::Binary,
        DataType::Date => Family::Date,
        DataType::Time { .. } => Family::Time,
        DataType::Timestamp { .. } | DataType::TimestampTz { .. } => Family::Timestamp,
        DataType::Bool => Family::Bool,
        DataType::Uuid => Family::Uuid,
        DataType::Json { .. } => Family::Json,
        DataType::Interval => Family::Interval,
        DataType::Array { .. } | DataType::Enum { .. } | DataType::Other { .. } => return None,
    })
}

fn is_integer(t: &DataType) -> bool {
    matches!(t, DataType::Int { .. })
}

/// The validator's state: issues in design order.
struct Validator<'a> {
    design: &'a TableDesign,
    input: &'a PreviewInput<'a>,
    caps: &'a DesignerCapabilities,
    issues: Vec<DesignIssue>,
    /// Some change rebuilds the table.
    rebuild: bool,
}

impl<'a> Validator<'a> {
    fn error(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.issues.push(DesignIssue::error(path, message));
    }

    fn warning(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.issues.push(DesignIssue::warning(path, message));
    }

    /// Records that `what` needs `support`: an error when unsupported, a rebuild note when it
    /// takes a rebuild.
    fn need(&mut self, support: Support, path: &str, what: &str) {
        match support {
            Support::Native => {}
            Support::Rebuild => self.rebuild = true,
            Support::Unsupported => {
                self.error(path, format!("{what} is not supported on this database"));
            }
        }
    }

    /// Empty and over-long names.
    fn name(&mut self, path: &str, what: &str, name: &str) {
        if name.trim().is_empty() {
            self.error(path, format!("{what} has no name"));
            return;
        }
        if let Some(max) = self.caps.max_identifier_length
            && name.len() > usize::try_from(max).unwrap_or(usize::MAX)
        {
            self.error(
                path,
                format!("the name '{name}' is longer than {max} bytes"),
            );
        }
    }

    /// A user-typed expression is one expression.
    fn expression(&mut self, path: &str, what: &str, text: &str) {
        if let Err(e) = check_expression(self.input.quoting, text) {
            let reason = match e {
                Error::InvalidInput { message } => message,
                other => other.to_string(),
            };
            self.error(path, format!("{what}: {reason}"));
        }
    }

    fn original(&self) -> Option<&'a TableDef> {
        self.design.original.as_ref()
    }

    /// Origins point into the original's list of `len` items, at most once each.
    fn origins(&mut self, list: &str, origins: &[Option<usize>], len: Option<usize>) {
        let mut seen = Vec::new();
        for (pos, origin) in origins.iter().enumerate() {
            let Some(origin) = *origin else { continue };
            let path = format!("{list}[{pos}]");
            match len {
                None => self.error(path, "a new table has no original items to point at"),
                Some(len) if origin >= len => {
                    self.error(path, format!("origin {origin} is out of range"));
                }
                Some(_) if seen.contains(&origin) => {
                    self.error(path, format!("origin {origin} is used twice"));
                }
                Some(_) => seen.push(origin),
            }
        }
    }

    /// Origins of the original's items that the design no longer has.
    fn removed(origins: &[Option<usize>], len: usize) -> usize {
        (0..len).filter(|i| !origins.contains(&Some(*i))).count()
    }

    fn table(&mut self) {
        let design = self.design;
        self.name("table.name", "the table", &design.table.name);
        let comment = non_empty(design.comment.as_deref());
        match self.original() {
            None => {
                if !self.caps.create_table {
                    self.error("table", "creating tables is not supported on this database");
                }
                if comment.is_some() {
                    self.need(self.caps.table_comment, "comment", "a table comment");
                }
            }
            Some(original) => {
                if original.obj.name != design.table.name {
                    self.need(self.caps.rename_table, "table.name", "renaming the table");
                }
                if non_empty(original.comment.as_deref()) != comment {
                    self.need(
                        self.caps.table_comment,
                        "comment",
                        "changing the table comment",
                    );
                }
            }
        }
    }

    fn columns(&mut self) {
        let design = self.design;
        if design.columns.is_empty() {
            self.error("columns", "the table has no columns");
        }
        let origins: Vec<Option<usize>> = design.columns.iter().map(|c| c.origin).collect();
        self.origins(
            "columns",
            &origins,
            self.original().map(|o| o.columns.len()),
        );
        let new_def = design.to_def(self.input.quoting);
        for (pos, (item, column)) in design.columns.iter().zip(&new_def.columns).enumerate() {
            let path = format!("columns[{pos}]");
            self.name(&format!("{path}.name"), "a column", &column.name);
            self.duplicate(
                &format!("{path}.name"),
                "column",
                &column.name,
                new_def.columns[..pos].iter().map(|c| c.name.as_str()),
            );
            self.column(&path, item, column);
        }
        if let Some(original) = self.original() {
            if Self::removed(&origins, original.columns.len()) > 0 {
                self.need(self.caps.drop_column, "columns", "dropping a column");
            }
            let kept: Vec<usize> = origins.iter().flatten().copied().collect();
            if kept.windows(2).any(|pair| pair[0] > pair[1]) {
                self.need(self.caps.reorder_columns, "columns", "reordering columns");
            }
        }
    }

    fn column(&mut self, path: &str, item: &DesignColumn, column: &ColumnDef) {
        let caps = self.caps;
        let old = item
            .origin
            .and_then(|origin| self.original().and_then(|o| o.columns.get(origin)));
        // What the server reported for an unchanged property is not the user's to fix: only
        // new and changed properties are checked against the engine.
        let generates_changed = old.is_none_or(|o| {
            o.auto_increment != column.auto_increment
                || o.identity != column.identity
                || o.data_type != column.data_type
        });
        if generates_changed
            && (column.auto_increment || column.identity.is_some())
            && !is_integer(&column.data_type)
        {
            self.error(
                path.to_owned(),
                format!(
                    "'{}' is not an integer column, so it cannot generate values",
                    column.name
                ),
            );
        }
        if generates_changed && column.identity.is_some() && !caps.identity {
            self.error(
                path.to_owned(),
                "identity columns are not supported on this database",
            );
        }
        if generates_changed
            && column.auto_increment
            && column.identity.is_none()
            && !caps.auto_increment
        {
            self.error(
                path.to_owned(),
                "auto-increment columns are not supported on this database",
            );
        }
        if item.default_literal.is_none()
            && let Some(default) = &column.default
            && old.is_none_or(|o| o.default.as_ref() != Some(default))
        {
            self.expression(&format!("{path}.default"), "the default", default);
        }
        if let Some(generated) = &column.generated
            && old.is_none_or(|o| o.generated.as_ref() != Some(generated))
        {
            self.expression(
                &format!("{path}.generated"),
                "the generation expression",
                &generated.expression,
            );
            let supported = if generated.stored {
                caps.generated_stored
            } else {
                caps.generated_virtual
            };
            if !supported {
                self.error(
                    format!("{path}.generated"),
                    if generated.stored {
                        "stored generated columns are not supported on this database"
                    } else {
                        "virtual generated columns are not supported on this database"
                    },
                );
            }
        }
        let Some(old) = old else {
            // A new column.
            if self.original().is_some() {
                self.need(caps.add_column, path, "adding a column");
                let fills = column.default.is_some()
                    || column.auto_increment
                    || column.identity.is_some()
                    || column.generated.is_some();
                if !column.nullable && !fills {
                    self.warning(
                        path.to_owned(),
                        format!(
                            "'{}' is NOT NULL without a default: adding it fails if the table \
                             has rows",
                            column.name
                        ),
                    );
                }
            }
            if non_empty(column.comment.as_deref()).is_some() {
                self.need(caps.column_comment, path, "a column comment");
            }
            if column.collation.is_some() {
                self.need(caps.column_collation, path, "a column collation");
            }
            return;
        };
        if old.name != column.name {
            self.need(caps.rename_column, path, "renaming a column");
        }
        let type_changed = old.data_type != column.data_type
            || !old
                .native_type
                .trim()
                .eq_ignore_ascii_case(column.native_type.trim());
        if type_changed {
            self.need(caps.alter_column_type, path, "changing a column's type");
            self.warning(
                format!("{path}.dataType"),
                format!(
                    "changing the type of '{}' may fail or truncate existing values",
                    column.name
                ),
            );
        }
        if old.nullable != column.nullable {
            self.need(
                caps.alter_column_nullability,
                path,
                "changing a column's nullability",
            );
        }
        if old.default != column.default {
            self.need(
                caps.alter_column_default,
                path,
                "changing a column's default",
            );
        }
        if non_empty(old.comment.as_deref()) != non_empty(column.comment.as_deref()) {
            self.need(caps.column_comment, path, "changing a column comment");
        }
        if column.collation.is_some() && old.collation != column.collation {
            self.need(caps.column_collation, path, "changing a column's collation");
        }
    }

    /// An error when `name` equals one of `earlier`; a warning when it differs only in case.
    fn duplicate<'n>(
        &mut self,
        path: &str,
        what: &str,
        name: &str,
        earlier: impl Iterator<Item = &'n str>,
    ) {
        let mut case_only = false;
        for other in earlier {
            if other == name {
                self.error(
                    path.to_owned(),
                    format!("there is already a {what} named '{name}'"),
                );
                return;
            }
            case_only |= other.eq_ignore_ascii_case(name);
        }
        if case_only && !name.is_empty() {
            self.warning(
                path.to_owned(),
                format!(
                    "another {what} name differs from '{name}' only in case; some databases \
                     treat them as the same name"
                ),
            );
        }
    }

    fn column_names(&self) -> Vec<String> {
        self.design
            .columns
            .iter()
            .map(|c| c.column.name.clone())
            .collect()
    }

    fn primary_key(&mut self) {
        let design = self.design;
        // Renaming a column the key uses is not a key change.
        let changed = self.original().is_none_or(|original| {
            let before: Option<Vec<String>> = original.primary_key.as_ref().map(|pk| {
                pk.columns
                    .iter()
                    .map(|c| renamed_column(design, original, c))
                    .collect()
            });
            before.as_ref() != design.primary_key.as_ref().map(|pk| &pk.columns)
        });
        if let Some(pk) = &design.primary_key {
            if pk.columns.is_empty() {
                self.error("primaryKey", "the primary key has no columns");
            }
            for (pos, name) in pk.columns.iter().enumerate() {
                let path = format!("primaryKey.columns[{pos}]");
                if pk.columns[..pos].contains(name) {
                    self.error(path, format!("'{name}' is in the primary key twice"));
                    continue;
                }
                match design.columns.iter().find(|c| c.column.name == *name) {
                    None => self.error(path, format!("there is no column '{name}'")),
                    // A key the server reported as it is stays acceptable (SQLite allows NULL
                    // in some primary keys).
                    Some(c) if c.column.nullable && (changed || self.nullability_changed(c)) => {
                        self.error(
                            path,
                            format!("primary key column '{name}' must be NOT NULL"),
                        );
                    }
                    Some(_) => {}
                }
            }
            if let Some(name) = &pk.name {
                self.name("primaryKey.name", "the primary key", name);
            }
        }
        if changed && self.original().is_some() {
            self.need(
                self.caps.change_primary_key,
                "primaryKey",
                "changing the primary key",
            );
        }
    }

    /// `true` for a new column, or one whose nullability differs from its origin's.
    fn nullability_changed(&self, column: &DesignColumn) -> bool {
        column
            .origin
            .and_then(|o| self.original().and_then(|t| t.columns.get(o)))
            .is_none_or(|old| old.nullable != column.column.nullable)
    }

    /// Names of every constraint and index so far (they share one namespace on most engines).
    fn constraint_names(&self) -> Vec<String> {
        let design = self.design;
        design
            .primary_key
            .as_ref()
            .and_then(|pk| pk.name.clone())
            .into_iter()
            .collect()
    }

    fn indexes(&mut self) {
        let design = self.design;
        let caps = self.caps;
        let origins: Vec<Option<usize>> = design.indexes.iter().map(|i| i.origin).collect();
        self.origins(
            "indexes",
            &origins,
            self.original().map(|o| o.indexes.len()),
        );
        let columns = self.column_names();
        let mut names = self.constraint_names();
        for (pos, item) in design.indexes.iter().enumerate() {
            let path = format!("indexes[{pos}]");
            let index = &item.def;
            self.name(&format!("{path}.name"), "an index", &index.name);
            self.duplicate(
                &format!("{path}.name"),
                "index or constraint",
                &index.name,
                names.iter().map(String::as_str),
            );
            names.push(index.name.clone());
            if index.columns.is_empty() {
                self.error(format!("{path}.columns"), "the index has no columns");
            }
            for (k, part) in index.columns.iter().enumerate() {
                if !part.is_expression && !columns.contains(&part.name) {
                    self.error(
                        format!("{path}.columns[{k}]"),
                        format!("there is no column '{}'", part.name),
                    );
                }
            }
            let original = item
                .origin
                .and_then(|o| self.original().and_then(|t| t.indexes.get(o)));
            if let Some(old) = original {
                let mut renamed = old.clone();
                renamed.name.clone_from(&index.name);
                if let Some(design_original) = self.original() {
                    for part in renamed.columns.iter_mut().filter(|p| !p.is_expression) {
                        part.name = renamed_column(design, design_original, &part.name);
                    }
                }
                if renamed == *index {
                    // Unchanged (or only renamed): the server's own definition.
                    continue;
                }
                self.need(caps.drop_index, &path, "changing an index");
                self.need(caps.add_index, &path, "changing an index");
            } else if self.original().is_some() {
                self.need(caps.add_index, &path, "adding an index");
            }
            for (k, part) in index.columns.iter().enumerate() {
                let part_path = format!("{path}.columns[{k}]");
                if part.is_expression {
                    if !caps.expression_indexes {
                        self.error(
                            part_path.clone(),
                            "expression keys are not supported on this database",
                        );
                    }
                    self.expression(&part_path, "the index expression", &part.name);
                }
                if part.descending && !caps.descending_indexes {
                    self.error(
                        part_path.clone(),
                        "descending index keys are not supported on this database",
                    );
                }
                if part.length.is_some() && !caps.index_prefix_lengths {
                    self.error(
                        part_path,
                        "index prefix lengths are not supported on this database",
                    );
                }
            }
            if let Some(predicate) = &index.predicate {
                if !caps.partial_indexes {
                    self.error(
                        format!("{path}.predicate"),
                        "partial indexes are not supported on this database",
                    );
                }
                self.expression(
                    &format!("{path}.predicate"),
                    "the index condition",
                    predicate,
                );
            }
            if index.is_constraint && !caps.unique_constraints {
                self.error(
                    path.clone(),
                    "unique constraints are not supported on this database",
                );
            }
        }
        if let Some(original) = self.original()
            && Self::removed(&origins, original.indexes.len()) > 0
        {
            self.need(caps.drop_index, "indexes", "dropping an index");
        }
    }

    fn foreign_keys(&mut self) {
        let design = self.design;
        let caps = self.caps;
        let origins: Vec<Option<usize>> = design.foreign_keys.iter().map(|i| i.origin).collect();
        self.origins(
            "foreignKeys",
            &origins,
            self.original().map(|o| o.foreign_keys.len()),
        );
        let mut names = self.constraint_names();
        names.extend(design.indexes.iter().map(|i| i.def.name.clone()));
        for (pos, item) in design.foreign_keys.iter().enumerate() {
            let path = format!("foreignKeys[{pos}]");
            let fk = &item.def;
            if let Some(name) = &fk.name {
                self.name(&format!("{path}.name"), "a foreign key", name);
                self.duplicate(
                    &format!("{path}.name"),
                    "index or constraint",
                    name,
                    names.iter().map(String::as_str),
                );
                names.push(name.clone());
            }
            if fk.columns.is_empty() {
                self.error(format!("{path}.columns"), "the foreign key has no columns");
            }
            if fk.columns.len() != fk.ref_columns.len() {
                self.error(
                    format!("{path}.refColumns"),
                    format!(
                        "the foreign key has {} columns but references {}",
                        fk.columns.len(),
                        fk.ref_columns.len()
                    ),
                );
            }
            for (k, name) in fk.columns.iter().enumerate() {
                if !design.columns.iter().any(|c| c.column.name == *name) {
                    self.error(
                        format!("{path}.columns[{k}]"),
                        format!("there is no column '{name}'"),
                    );
                }
            }
            if fk.ref_table.name.trim().is_empty() {
                self.error(
                    format!("{path}.refTable"),
                    "the foreign key references no table",
                );
            }
            let original = item
                .origin
                .and_then(|o| self.original().and_then(|t| t.foreign_keys.get(o)));
            if let Some(old) = original {
                let mut renamed = old.clone();
                renamed.name.clone_from(&fk.name);
                if let Some(design_original) = self.original() {
                    for column in &mut renamed.columns {
                        *column = renamed_column(design, design_original, column);
                    }
                }
                if renamed == *fk {
                    // Unchanged (or only renamed): the server's own definition.
                    continue;
                }
                self.need(caps.drop_foreign_key, &path, "changing a foreign key");
                self.need(caps.add_foreign_key, &path, "changing a foreign key");
            } else if self.original().is_some() {
                self.need(caps.add_foreign_key, &path, "adding a foreign key");
            }
            if references_itself(design, fk) {
                // A self-reference: the target columns are this design's.
                for (k, (from, to)) in fk.columns.iter().zip(&fk.ref_columns).enumerate() {
                    let target = design.columns.iter().find(|c| c.column.name == *to);
                    let source = design.columns.iter().find(|c| c.column.name == *from);
                    match (source, target) {
                        (_, None) => self.error(
                            format!("{path}.refColumns[{k}]"),
                            format!("the table has no column '{to}'"),
                        ),
                        (Some(s), Some(t))
                            if !compatible_types(&s.column.data_type, &t.column.data_type) =>
                        {
                            self.error(
                                format!("{path}.refColumns[{k}]"),
                                format!(
                                    "'{from}' ({}) cannot reference '{to}' ({})",
                                    s.column.data_type, t.column.data_type
                                ),
                            );
                        }
                        _ => {}
                    }
                }
            }
            for (action, which) in [(fk.on_delete, "onDelete"), (fk.on_update, "onUpdate")] {
                if action != FkAction::NoAction && !caps.fk_actions.contains(&action) {
                    self.error(
                        format!("{path}.{which}"),
                        format!("the action {action:?} is not supported on this database"),
                    );
                }
            }
        }
        if let Some(original) = self.original()
            && Self::removed(&origins, original.foreign_keys.len()) > 0
        {
            self.need(
                caps.drop_foreign_key,
                "foreignKeys",
                "dropping a foreign key",
            );
        }
    }

    fn checks(&mut self) {
        let design = self.design;
        let caps = self.caps;
        let origins: Vec<Option<usize>> = design.checks.iter().map(|i| i.origin).collect();
        self.origins("checks", &origins, self.original().map(|o| o.checks.len()));
        let mut names = self.constraint_names();
        names.extend(design.indexes.iter().map(|i| i.def.name.clone()));
        names.extend(
            design
                .foreign_keys
                .iter()
                .filter_map(|f| f.def.name.clone()),
        );
        for (pos, item) in design.checks.iter().enumerate() {
            let path = format!("checks[{pos}]");
            let check = &item.def;
            if let Some(name) = &check.name {
                self.name(&format!("{path}.name"), "a check", name);
                self.duplicate(
                    &format!("{path}.name"),
                    "index or constraint",
                    name,
                    names.iter().map(String::as_str),
                );
                names.push(name.clone());
            }
            if !caps.checks_enforced {
                self.warning(
                    path.clone(),
                    "this database does not enforce check constraints",
                );
            }
            let original = item
                .origin
                .and_then(|o| self.original().and_then(|t| t.checks.get(o)));
            if let Some(old) = original {
                let mut renamed = old.clone();
                renamed.name.clone_from(&check.name);
                if renamed == *check {
                    // Unchanged (or only renamed): the server's own text.
                    continue;
                }
                self.need(caps.drop_check, &path, "changing a check");
                self.need(caps.add_check, &path, "changing a check");
            } else if self.original().is_some() {
                self.need(caps.add_check, &path, "adding a check");
            }
            if check.expression.trim().is_empty() {
                self.error(format!("{path}.expression"), "the check has no expression");
            } else {
                self.expression(
                    &format!("{path}.expression"),
                    "the check",
                    &check.expression,
                );
            }
        }
        if let Some(original) = self.original()
            && Self::removed(&origins, original.checks.len()) > 0
        {
            self.need(caps.drop_check, "checks", "dropping a check");
        }
    }
}

/// `Some(text)` unless absent or blank (absent and empty comments are equal).
fn non_empty(text: Option<&str>) -> Option<&str> {
    text.filter(|t| !t.trim().is_empty())
}

/// The design's name for the original's column `name` (after a rename), else `name`.
fn renamed_column(design: &TableDesign, original: &TableDef, name: &str) -> String {
    let Some(index) = original.columns.iter().position(|c| c.name == name) else {
        return name.to_owned();
    };
    design
        .columns
        .iter()
        .find(|c| c.origin == Some(index))
        .map_or_else(|| name.to_owned(), |c| c.column.name.clone())
}

/// `fk` references the design's own table (same name; same schema and catalog where set).
pub fn references_itself(design: &TableDesign, fk: &ForeignKeyDef) -> bool {
    let target = &fk.ref_table;
    let table = &design.table;
    let same = |a: Option<&str>, b: Option<&str>| match (a, b) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    };
    let names_match = target.name == table.name
        || design
            .original
            .as_ref()
            .is_some_and(|o| o.obj.name == target.name);
    names_match
        && same(target.schema.as_deref(), table.schema.as_deref())
        && same(target.catalog.as_deref(), table.catalog.as_deref())
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

    // -----------------------------------------------------------------------------------------
    // validate / preview / render_script
    // -----------------------------------------------------------------------------------------

    use oxbase_dialect::compare::{DefaultRules, RulesComparator};
    use oxbase_types::{GeneratedColumn, IdentityDef, ScriptStep};

    /// Echoes what changed: renames, drops (destructive), adds; one step per change.
    struct ToyDdl {
        transactional: bool,
    }

    impl ToyDdl {
        fn step(&self, sql: String) -> ScriptStep {
            let step = ScriptStep::new(sql);
            if self.transactional {
                step
            } else {
                step.non_transactional()
            }
        }
    }

    impl DdlGenerator for ToyDdl {
        fn create(&self, d: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
            let ObjectDef::Table(t) = d else {
                return Err(Error::unsupported("tables only"));
            };
            assert!(ctx.table.is_none());
            Ok([self.step(format!("CREATE TABLE {}", t.obj.name))]
                .into_iter()
                .collect())
        }

        fn alter(&self, old: &ObjectDef, new: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
            let (ObjectDef::Table(a), ObjectDef::Table(b)) = (old, new) else {
                return Err(Error::unsupported("tables only"));
            };
            assert_eq!(ctx.table, Some(a));
            let mut script = Script::new();
            for r in ctx.renames {
                script.push_step(self.step(format!("RENAME {} {} TO {}", r.kind, r.old, r.new)));
            }
            let renamed = apply_renames(a, ctx.renames);
            for c in &renamed.columns {
                if b.column(&c.name).is_none() {
                    script.push_step(self.step(format!("DROP {}", c.name)).destructive());
                }
            }
            for c in &b.columns {
                if renamed.column(&c.name).is_none() {
                    script.push_step(self.step(format!("ADD {}", c.name)));
                }
            }
            if renamed.checks != b.checks {
                script.push_step(self.step("CHECKS".to_owned()));
            }
            script.warn("toy warning");
            Ok(script)
        }

        fn drop_object(&self, r: &ObjectRef, _ctx: &DdlContext<'_>) -> Result<Script> {
            Ok(Script::single(format!("DROP TABLE {}", r.name)))
        }
    }

    fn caps(transactional: bool) -> DesignerCapabilities {
        DesignerCapabilities {
            transactional_ddl: transactional,
            create_table: true,
            rename_table: Support::Native,
            add_column: Support::Native,
            drop_column: Support::Native,
            rename_column: Support::Native,
            alter_column_type: Support::Native,
            alter_column_nullability: Support::Native,
            alter_column_default: Support::Native,
            column_comment: Support::Native,
            table_comment: Support::Native,
            change_primary_key: Support::Native,
            add_index: Support::Native,
            drop_index: Support::Native,
            unique_constraints: true,
            auto_increment: true,
            add_foreign_key: Support::Native,
            drop_foreign_key: Support::Native,
            fk_actions: vec![FkAction::Cascade, FkAction::Restrict],
            add_check: Support::Native,
            drop_check: Support::Native,
            checks_enforced: true,
            max_identifier_length: Some(63),
            notes: vec!["engine note".into()],
            ..DesignerCapabilities::default()
        }
    }

    struct Engine {
        ddl: ToyDdl,
        comparator: RulesComparator<DefaultRules>,
        caps: DesignerCapabilities,
        version: ServerVersion,
    }

    impl Engine {
        fn new(transactional: bool) -> Self {
            Self {
                ddl: ToyDdl { transactional },
                comparator: RulesComparator(DefaultRules),
                caps: caps(transactional),
                version: ServerVersion::new(17, 0, 0),
            }
        }

        fn input(&self) -> PreviewInput<'_> {
            PreviewInput {
                ddl: &self.ddl,
                comparator: &self.comparator,
                quoting: &PgQuoting,
                capabilities: &self.caps,
                version: &self.version,
                triggers: &[],
                terminator: ";",
            }
        }
    }

    fn issues(design: &TableDesign, engine: &Engine) -> Vec<(IssueSeverity, String, String)> {
        validate(design, &engine.input())
            .into_iter()
            .map(|i| (i.severity, i.path, i.message))
            .collect()
    }

    /// `true` when an issue of `severity` at `path` mentions `text`.
    fn has(
        found: &[(IssueSeverity, String, String)],
        severity: IssueSeverity,
        path: &str,
        text: &str,
    ) -> bool {
        found
            .iter()
            .any(|(s, p, m)| *s == severity && p == path && m.contains(text))
    }

    fn new_column(name: &str, data_type: DataType) -> DesignColumn {
        DesignColumn {
            origin: None,
            column: ColumnDef::new(name, data_type),
            default_literal: None,
        }
    }

    #[test]
    fn a_loaded_design_is_valid() {
        let engine = Engine::new(true);
        let design = TableDesign::from_def(loaded());
        assert_eq!(issues(&design, &engine), []);
    }

    #[test]
    fn validation_errors_and_warnings() {
        use IssueSeverity::{Error as E, Warning as W};
        let engine = Engine::new(true);
        let mut d = TableDesign::from_def(loaded());
        d.table.name = "x".repeat(64);
        d.columns.push(new_column("ID", DataType::Text));
        d.columns.push(new_column("customer", DataType::Text));
        let mut identity = new_column("serial_text", DataType::Text);
        identity.column.identity = Some(IdentityDef {
            always: true,
            start: None,
            increment: None,
        });
        d.columns.push(identity);
        let mut bad_default = new_column("d", DataType::INT32);
        bad_default.column.default = Some("1; DROP TABLE x".into());
        d.columns.push(bad_default);
        let mut required = new_column("req", DataType::INT32);
        required.column.nullable = false;
        d.columns.push(required);
        let mut generated = new_column("g", DataType::INT32);
        generated.column.generated = Some(GeneratedColumn {
            expression: "customer * 2".into(),
            stored: true,
        });
        d.columns.push(generated);
        d.columns[1].column.data_type = DataType::INT64;
        d.primary_key = Some(PrimaryKeyDef {
            name: None,
            columns: vec!["id".into(), "nope".into(), "customer".into()],
        });
        d.indexes.push(DesignItem::new(IndexDef {
            name: "orders_customer_idx".into(),
            columns: vec![IndexColumn {
                name: "lower(x)".into(),
                is_expression: true,
                descending: true,
                length: Some(3),
            }],
            unique: false,
            is_constraint: false,
            predicate: Some("x > 0 -- tail".into()),
            extras: Extras::new(),
        }));
        d.foreign_keys.push(DesignItem::new(ForeignKeyDef {
            name: Some("self_fk".into()),
            columns: vec!["customer".into(), "id".into()],
            ref_table: ObjectRef::table(None, Some("public"), "orders"),
            ref_columns: vec!["id".into()],
            on_delete: FkAction::SetDefault,
            on_update: FkAction::NoAction,
            extras: Extras::new(),
        }));
        d.foreign_keys.push(DesignItem::new(ForeignKeyDef {
            name: Some("self_fk2".into()),
            columns: vec!["serial_text".into()],
            ref_table: ObjectRef::table(None, Some("public"), "orders"),
            ref_columns: vec!["id".into()],
            on_delete: FkAction::NoAction,
            on_update: FkAction::NoAction,
            extras: Extras::new(),
        }));
        d.checks.push(DesignItem::new(CheckDef {
            name: Some("orders_customer_fk".into()),
            expression: " ".into(),
            extras: Extras::new(),
        }));
        d.checks[0].origin = Some(7);
        let found = issues(&d, &engine);
        for (severity, path, text) in [
            (E, "table.name", "longer than 63 bytes"),
            (W, "columns[2].name", "only in case"),
            (E, "columns[3].name", "already a column named 'customer'"),
            (E, "columns[4]", "not an integer column"),
            (E, "columns[4]", "identity columns are not supported"),
            (E, "columns[5].default", "contains ';'"),
            (W, "columns[6]", "NOT NULL without a default"),
            (
                E,
                "columns[7].generated",
                "stored generated columns are not supported",
            ),
            (W, "columns[1].dataType", "may fail or truncate"),
            (E, "primaryKey.columns[1]", "no column 'nope'"),
            (E, "primaryKey.columns[2]", "must be NOT NULL"),
            (E, "indexes[1].name", "already a index or constraint named"),
            (
                E,
                "indexes[1].columns[0]",
                "expression keys are not supported",
            ),
            (
                E,
                "indexes[1].columns[0]",
                "descending index keys are not supported",
            ),
            (
                E,
                "indexes[1].columns[0]",
                "prefix lengths are not supported",
            ),
            (
                E,
                "indexes[1].predicate",
                "partial indexes are not supported",
            ),
            (E, "indexes[1].predicate", "contains a comment"),
            (
                E,
                "foreignKeys[1].refColumns",
                "has 2 columns but references 1",
            ),
            (E, "foreignKeys[1].onDelete", "not supported"),
            (E, "foreignKeys[2].refColumns[0]", "cannot reference 'id'"),
            (E, "checks[0]", "out of range"),
            (E, "checks[1].name", "already a index or constraint named"),
            (E, "checks[1].expression", "has no expression"),
        ] {
            assert!(
                has(&found, severity, path, text),
                "missing {severity:?} {path}: {text}\nin {found:#?}"
            );
        }
        // Design order: table, columns, primary key, indexes, foreign keys, checks.
        let first_pk = found
            .iter()
            .position(|(_, p, _)| p.starts_with("primaryKey"));
        let last_column = found.iter().rposition(|(_, p, _)| p.starts_with("columns"));
        assert!(last_column < first_pk);
    }

    #[test]
    fn checks_need_the_engine() {
        let mut engine = Engine::new(true);
        let mut d = TableDesign::from_def(loaded());
        // Drop a column and rename the table on an engine that cannot.
        d.columns.pop();
        d.table.name = "orders2".into();
        engine.caps.drop_column = Support::Unsupported;
        engine.caps.rename_table = Support::Rebuild;
        engine.caps.checks_enforced = false;
        let found = issues(&d, &engine);
        assert!(has(
            &found,
            IssueSeverity::Error,
            "columns",
            "dropping a column is not supported"
        ));
        assert!(has(
            &found,
            IssueSeverity::Warning,
            "checks[0]",
            "does not enforce"
        ));
        assert!(has(
            &found,
            IssueSeverity::Warning,
            "table",
            "rebuilds the table"
        ));
        // Origins: duplicates are errors.
        let mut d = TableDesign::from_def(loaded());
        d.columns[1].origin = Some(0);
        let found = issues(&d, &Engine::new(true));
        assert!(has(
            &found,
            IssueSeverity::Error,
            "columns[1]",
            "used twice"
        ));
        // A new table with origins.
        let mut d = TableDesign::new_table(ObjectRef::table(None, None, "t"));
        d.columns.push(DesignColumn {
            origin: Some(0),
            column: ColumnDef::new("a", DataType::INT32),
            default_literal: None,
        });
        let found = issues(&d, &Engine::new(true));
        assert!(has(
            &found,
            IssueSeverity::Error,
            "columns[0]",
            "no original items"
        ));
        let empty = TableDesign::new_table(ObjectRef::table(None, None, ""));
        let found = issues(&empty, &Engine::new(true));
        assert!(has(
            &found,
            IssueSeverity::Error,
            "table.name",
            "has no name"
        ));
        assert!(has(&found, IssueSeverity::Error, "columns", "no columns"));
    }

    #[test]
    fn previews_report_renames_and_the_script() {
        let engine = Engine::new(true);
        let mut d = TableDesign::from_def(loaded());
        d.columns[1].column.name = "customer_id".into();
        // The UI renames the column where the index and the foreign key use it.
        d.indexes[0].def.columns[0].name = "customer_id".into();
        d.foreign_keys[0].def.columns = vec!["customer_id".into()];
        d.columns.push(new_column("note", DataType::Text));
        let p = preview(&d, &engine.input()).unwrap();
        assert!(!p.has_errors(), "{:?}", p.issues);
        // A rename is a `name` difference, not a drop plus an add.
        assert_eq!(p.diffs[0].prop, Some(props::NAME));
        assert_eq!(p.diffs[0].old, Some(Value::Text("customer".into())));
        assert_eq!(p.diffs[0].new, Some(Value::Text("customer_id".into())));
        assert!(
            p.diffs
                .iter()
                .all(|diff| diff.kind != oxbase_types::DiffKind::Removed),
            "{:?}",
            p.diffs
        );
        assert_eq!(
            p.script
                .steps
                .iter()
                .map(|s| s.sql.as_str())
                .collect::<Vec<_>>(),
            ["RENAME column customer TO customer_id", "ADD note"]
        );
        assert!(p.atomic && !p.destructive);
        assert_eq!(p.notes, ["engine note", "toy warning"]);
        assert_eq!(
            p.sql,
            "BEGIN;\nRENAME column customer TO customer_id;\nADD note;\nCOMMIT;\n"
        );

        // A drop is destructive; without transactional DDL the preview says it is not atomic.
        let engine = Engine::new(false);
        let mut d = TableDesign::from_def(loaded());
        d.columns.pop();
        d.indexes.clear();
        d.foreign_keys.clear();
        d.checks.clear();
        let p = preview(&d, &engine.input()).unwrap();
        assert!(p.destructive && !p.atomic);
        assert!(
            p.notes.contains(&NOT_ATOMIC_NOTE.to_owned()),
            "{:?}",
            p.notes
        );
        assert!(!p.sql.contains("BEGIN"));

        // No change: no differences, no script.
        let p = preview(&TableDesign::from_def(loaded()), &engine.input()).unwrap();
        assert_eq!(p.diffs, []);
        assert!(p.script.is_empty() && p.sql.is_empty());

        // An error: no script.
        let mut d = TableDesign::from_def(loaded());
        d.checks[0].def.expression = "a; b".into();
        let p = preview(&d, &engine.input()).unwrap();
        assert!(p.has_errors() && p.script.is_empty() && p.sql.is_empty());

        // A new table: one `added` difference and the create script.
        let mut d = TableDesign::new_table(ObjectRef::table(Some("db"), Some("public"), "fresh"));
        d.columns.push(new_column("a", DataType::INT32));
        let p = preview(&d, &Engine::new(true).input()).unwrap();
        assert_eq!(p.diffs.len(), 1);
        assert_eq!(p.diffs[0].kind, oxbase_types::DiffKind::Added);
        assert_eq!(p.sql, "BEGIN;\nCREATE TABLE fresh;\nCOMMIT;\n");
    }

    #[test]
    fn render_script_marks_phases_and_transactions() {
        let mut script = Script::new();
        script
            .push_step(
                ScriptStep::new("PRAGMA foreign_keys = ON")
                    .in_phase(StepPhase::Finally)
                    .only_if("fk", "1")
                    .described("Restore foreign keys"),
            )
            .push_step(
                ScriptStep::new("PRAGMA foreign_keys")
                    .in_phase(StepPhase::Prepare)
                    .capture("fk"),
            )
            .push_step(
                ScriptStep::new("DROP TABLE \"t\"")
                    .described("Drop the old\ntable")
                    .destructive(),
            )
            .push_step(
                ScriptStep::new("PRAGMA foreign_key_check(\"t\")")
                    .expect_no_rows()
                    .only_if("fk", "1"),
            );
        assert_eq!(
            render_script(&script, true, ";"),
            "-- before the transaction\n\
             PRAGMA foreign_keys;\n\
             BEGIN;\n\
             -- Drop the old table (can lose data)\n\
             DROP TABLE \"t\";\n\
             -- only if fk = '1'; must return no rows\n\
             PRAGMA foreign_key_check(\"t\");\n\
             COMMIT;\n\
             -- Restore foreign keys (after the transaction, always; only if fk = '1')\n\
             PRAGMA foreign_keys = ON;\n"
        );
        let mut plain = Script::new();
        plain.push("ALTER TABLE t ADD c int");
        assert_eq!(
            render_script(&plain, false, "\nGO"),
            "ALTER TABLE t ADD c int\nGO\n"
        );
        assert_eq!(
            render_script(&plain, true, ";"),
            "BEGIN;\nALTER TABLE t ADD c int;\nCOMMIT;\n"
        );
        assert_eq!(render_script(&Script::new(), true, ";"), "");
    }

    #[test]
    fn type_families() {
        assert!(compatible_types(&DataType::INT32, &DataType::INT64));
        assert!(compatible_types(
            &DataType::VarChar { length: Some(5) },
            &DataType::Text
        ));
        assert!(!compatible_types(&DataType::Text, &DataType::INT64));
        assert!(compatible_types(
            &DataType::Other {
                name: "citext".into()
            },
            &DataType::Text
        ));
        assert!(!compatible_types(&DataType::Date, &DataType::Uuid));
    }
}
