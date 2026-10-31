//! The typed object model: one definition struct per object kind.
//!
//! M0 typed tables, columns, indexes, foreign keys, checks and views. M2 adds databases and
//! schemas ([`SchemaDef`]), functions and procedures ([`RoutineDef`]), triggers
//! ([`TriggerDef`]), sequences ([`SequenceDef`]) and column identity ([`IdentityDef`]). Kinds
//! without a struct yet (users, roles, grants, events, types, extensions) use [`GenericDef`]
//! (name + native definition text + extras). Every definition carries an [`Extras`] map for
//! database-specific properties, which the dialect `Capabilities` trait gates by engine and
//! server version.
//!
//! Text that is engine-native SQL (defaults, expressions, bodies, type names) is kept exactly as
//! the server reports it; the model never re-formats it.
//!
//! Serde: snake_case field names, `kind`-tagged [`ObjectDef`]. Integers that may not fit a
//! JavaScript number (sequence bounds) serialize as decimal strings, see [`SequenceDef`].

use serde::{Deserialize, Serialize};

use crate::{DataType, Extras, ObjectKind, ObjectRef};

/// A typed definition of one database object.
///
/// Which variant a fetcher returns for each [`ObjectKind`] (M2 contract):
///
/// | Kind | Variant |
/// |---|---|
/// | `Database`, `Schema` | [`ObjectDef::Schema`] (`obj.kind` tells which) |
/// | `Table` | [`ObjectDef::Table`] |
/// | `Column` / `Index` / `ForeignKey` / `Check` | the matching variant with its owning `table` |
/// | `View`, `MatView` | [`ObjectDef::View`] |
/// | `Function`, `Procedure` | [`ObjectDef::Routine`] |
/// | `Trigger` | [`ObjectDef::Trigger`] |
/// | `Sequence` | [`ObjectDef::Sequence`] |
/// | anything else | [`ObjectDef::Generic`] |
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObjectDef {
    /// A database or a schema (`obj.kind` is `Database` or `Schema`).
    Schema(SchemaDef),
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
    /// A function or procedure (`obj.kind` is `Function` or `Procedure`).
    Routine(RoutineDef),
    Trigger(TriggerDef),
    Sequence(SequenceDef),
    /// Any kind without a dedicated struct yet (user, role, grant, event, type, extension).
    Generic(GenericDef),
}

impl ObjectDef {
    /// The kind of object this definition describes.
    pub fn kind(&self) -> ObjectKind {
        match self {
            Self::Schema(d) => d.obj.kind,
            Self::Table(_) => ObjectKind::Table,
            Self::Column { .. } => ObjectKind::Column,
            Self::Index { .. } => ObjectKind::Index,
            Self::ForeignKey { .. } => ObjectKind::ForeignKey,
            Self::Check { .. } => ObjectKind::Check,
            Self::View(v) => v.obj.kind,
            Self::Routine(r) => r.obj.kind,
            Self::Trigger(_) => ObjectKind::Trigger,
            Self::Sequence(_) => ObjectKind::Sequence,
            Self::Generic(g) => g.obj.kind,
        }
    }

    /// The object's own name (may be empty for unnamed constraints).
    pub fn name(&self) -> &str {
        match self {
            Self::Schema(d) => &d.obj.name,
            Self::Table(t) => &t.obj.name,
            Self::Column { column, .. } => &column.name,
            Self::Index { index, .. } => &index.name,
            Self::ForeignKey { foreign_key, .. } => foreign_key.name.as_deref().unwrap_or(""),
            Self::Check { check, .. } => check.name.as_deref().unwrap_or(""),
            Self::View(v) => &v.obj.name,
            Self::Routine(r) => &r.obj.name,
            Self::Trigger(t) => &t.obj.name,
            Self::Sequence(q) => &q.obj.name,
            Self::Generic(g) => &g.obj.name,
        }
    }

    /// The path of the object this definition describes.
    pub fn object_ref(&self) -> ObjectRef {
        match self {
            Self::Schema(d) => d.obj.clone(),
            Self::Table(t) => t.obj.clone(),
            Self::View(v) => v.obj.clone(),
            Self::Routine(r) => r.obj.clone(),
            Self::Trigger(t) => t.obj.clone(),
            Self::Sequence(q) => q.obj.clone(),
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
            Self::Schema(d) => &d.extras,
            Self::Table(t) => &t.extras,
            Self::Column { column, .. } => &column.extras,
            Self::Index { index, .. } => &index.extras,
            Self::ForeignKey { foreign_key, .. } => &foreign_key.extras,
            Self::Check { check, .. } => &check.extras,
            Self::View(v) => &v.extras,
            Self::Routine(r) => &r.extras,
            Self::Trigger(t) => &t.extras,
            Self::Sequence(q) => &q.extras,
            Self::Generic(g) => &g.extras,
        }
    }

    /// Mutable access to the extras map.
    pub fn extras_mut(&mut self) -> &mut Extras {
        match self {
            Self::Schema(d) => &mut d.extras,
            Self::Table(t) => &mut t.extras,
            Self::Column { column, .. } => &mut column.extras,
            Self::Index { index, .. } => &mut index.extras,
            Self::ForeignKey { foreign_key, .. } => &mut foreign_key.extras,
            Self::Check { check, .. } => &mut check.extras,
            Self::View(v) => &mut v.extras,
            Self::Routine(r) => &mut r.extras,
            Self::Trigger(t) => &mut t.extras,
            Self::Sequence(q) => &mut q.extras,
            Self::Generic(g) => &mut g.extras,
        }
    }
}

/// A database or a schema (`obj.kind` is `Database` or `Schema`).
///
/// Engine facts such as the character set, collation or encoding go in `extras`
/// ([`crate::props::CHARSET`], [`crate::props::COLLATION`], [`crate::props::ENCODING`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchemaDef {
    #[serde(rename = "ref")]
    pub obj: ObjectRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(default)]
    pub extras: Extras,
}

impl SchemaDef {
    pub fn new(obj: ObjectRef) -> Self {
        Self {
            obj,
            owner: None,
            comment: None,
            extras: Extras::default(),
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
    /// The engine generates values for this column when none is given: MySQL/MariaDB
    /// `AUTO_INCREMENT`, SQLite `INTEGER PRIMARY KEY` rowid alias (and `AUTOINCREMENT`),
    /// PostgreSQL `serial` (a `nextval` default on an owned sequence) and identity columns.
    #[serde(default)]
    pub auto_increment: bool,
    /// SQL-standard identity details (PostgreSQL 10+ `GENERATED ... AS IDENTITY`, SQL Server
    /// `IDENTITY`). `Some` implies `auto_increment`. `None` for every other mechanism.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<IdentityDef>,
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
            identity: None,
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

/// An identity column (`GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityDef {
    /// `true` for `GENERATED ALWAYS`, `false` for `BY DEFAULT`.
    pub always: bool,
    /// Sequence options of the identity, when the engine reports them.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::int_text::opt"
    )]
    pub start: Option<i128>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::int_text::opt"
    )]
    pub increment: Option<i128>,
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

/// A function or procedure (`obj.kind` is `Function` or `Procedure`).
///
/// `obj.signature` carries the identity arguments on engines with overloading (PostgreSQL), see
/// [`ObjectRef`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutineDef {
    #[serde(rename = "ref")]
    pub obj: ObjectRef,
    /// Arguments in declaration order.
    #[serde(default)]
    pub args: Vec<RoutineArg>,
    /// Return type as engine-native text (`integer`, `SETOF record`, `TABLE(a int)`); `None`
    /// for procedures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns: Option<String>,
    /// Implementation language as the engine names it (`plpgsql`, `sql`, `c`); `SQL` for
    /// MySQL/MariaDB stored routines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// The body only (PostgreSQL `prosrc`, MySQL `ROUTINE_DEFINITION`), engine-native.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// The complete `CREATE ...` statement when the engine can produce it
    /// (`pg_get_functiondef`, `SHOW CREATE FUNCTION`). What the UI shows as "DDL".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Engine facts: [`crate::props::DETERMINISTIC`], [`crate::props::VOLATILITY`],
    /// [`crate::props::SECURITY`], [`crate::props::DEFINER`], [`crate::props::SQL_DATA_ACCESS`],
    /// ...
    #[serde(default)]
    pub extras: Extras,
}

impl RoutineDef {
    pub fn new(obj: ObjectRef) -> Self {
        Self {
            obj,
            args: Vec::new(),
            returns: None,
            language: None,
            body: None,
            definition: None,
            comment: None,
            extras: Extras::default(),
        }
    }
}

/// Direction of a routine argument.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgMode {
    #[default]
    In,
    Out,
    InOut,
    /// PostgreSQL `VARIADIC`.
    Variadic,
    /// A column of a PostgreSQL `RETURNS TABLE (...)` (listed as an argument by the catalog).
    Table,
}

/// One routine argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineArg {
    /// `None` for unnamed arguments (allowed in PostgreSQL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub mode: ArgMode,
    /// Type exactly as the engine spells it.
    pub native_type: String,
    /// Default as an engine-native expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// When a trigger fires relative to its event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerTiming {
    Before,
    After,
    InsteadOf,
}

/// An event a trigger fires on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerEvent {
    Insert,
    Update,
    Delete,
    /// PostgreSQL `TRUNCATE`.
    Truncate,
}

/// Row-level or statement-level trigger.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerLevel {
    #[default]
    Row,
    Statement,
}

/// A trigger. `obj` is a table child: `obj.parent` / `obj.parent_kind` name the table or view
/// it is defined on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerDef {
    #[serde(rename = "ref")]
    pub obj: ObjectRef,
    pub timing: TriggerTiming,
    /// One or more events, sorted (`Insert`, `Update`, `Delete`, `Truncate`).
    pub events: Vec<TriggerEvent>,
    /// `UPDATE OF col, ...` columns; empty when the trigger fires on any update.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub update_columns: Vec<String>,
    #[serde(default)]
    pub level: TriggerLevel,
    /// `WHEN (...)` condition, engine-native, without the `WHEN` keyword and parentheses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// The trigger function a PostgreSQL trigger executes (`EXECUTE FUNCTION f()`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<ObjectRef>,
    /// The action body for engines that inline it (MySQL/MariaDB `BEGIN ... END` or single
    /// statement, SQLite `BEGIN ... END`), engine-native.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// The complete `CREATE TRIGGER` statement when the engine can produce it
    /// (`pg_get_triggerdef`, `SHOW CREATE TRIGGER`, `sqlite_schema.sql`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    /// `false` when the trigger is disabled (PostgreSQL `ALTER TABLE ... DISABLE TRIGGER`).
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(default)]
    pub extras: Extras,
}

fn default_true() -> bool {
    true
}

/// A sequence.
///
/// Numbers are `i128` so MariaDB unsigned sequences and every PostgreSQL bound fit; they
/// serialize as decimal **strings** (`"9223372036854775807"`) because JSON consumers such as the
/// webview would round them to a double.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SequenceDef {
    #[serde(rename = "ref")]
    pub obj: ObjectRef,
    /// Value type as the engine spells it (`bigint`, `integer`), when the engine has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
    #[serde(with = "crate::int_text")]
    pub start: i128,
    #[serde(with = "crate::int_text")]
    pub increment: i128,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::int_text::opt"
    )]
    pub min_value: Option<i128>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::int_text::opt"
    )]
    pub max_value: Option<i128>,
    /// Values cached per session (`CACHE n`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::int_text::opt"
    )]
    pub cache: Option<i128>,
    #[serde(default)]
    pub cycle: bool,
    /// The column that owns the sequence (PostgreSQL `OWNED BY`, serial and identity columns):
    /// a `Column` ref with `parent` = the table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<ObjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(default)]
    pub extras: Extras,
}

impl SequenceDef {
    /// A sequence starting at 1 with increment 1 and engine-default bounds.
    pub fn new(obj: ObjectRef) -> Self {
        Self {
            obj,
            data_type: None,
            start: 1,
            increment: 1,
            min_value: None,
            max_value: None,
            cache: None,
            cycle: false,
            owned_by: None,
            comment: None,
            extras: Extras::default(),
        }
    }
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

    fn round_trip(def: &ObjectDef) -> serde_json::Value {
        let json = serde_json::to_value(def).unwrap();
        let back: ObjectDef = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(&back, def);
        json
    }

    #[test]
    fn m2_models_round_trip_and_report_their_kind() {
        let schema = ObjectRef::schema(Some("oxbase_test"), "m2");

        let s = ObjectDef::Schema(SchemaDef::new(schema.clone()));
        assert_eq!(s.kind(), ObjectKind::Schema);
        assert_eq!(round_trip(&s)["kind"], "schema");
        let db = ObjectDef::Schema(SchemaDef::new(ObjectRef::database("oxbase_test")));
        assert_eq!(db.kind(), ObjectKind::Database);

        let mut f = RoutineDef::new(
            schema
                .child(ObjectKind::Function, "m2_fn")
                .with_signature("integer"),
        );
        f.args.push(RoutineArg {
            name: Some("n".into()),
            mode: ArgMode::In,
            native_type: "integer".into(),
            default: None,
        });
        f.returns = Some("integer".into());
        f.language = Some("sql".into());
        let f = ObjectDef::Routine(f);
        assert_eq!(f.kind(), ObjectKind::Function);
        assert_eq!(f.object_ref().signature.as_deref(), Some("integer"));
        assert_eq!(round_trip(&f)["kind"], "routine");

        let table = schema.child(ObjectKind::Table, "m2_child");
        let t = ObjectDef::Trigger(TriggerDef {
            obj: table.child(ObjectKind::Trigger, "m2_trg"),
            timing: TriggerTiming::Before,
            events: vec![TriggerEvent::Insert, TriggerEvent::Update],
            update_columns: vec![],
            level: TriggerLevel::Row,
            condition: None,
            function: Some(
                schema
                    .child(ObjectKind::Function, "m2_trg_fn")
                    .with_signature(""),
            ),
            body: None,
            definition: None,
            enabled: true,
            comment: None,
            extras: Extras::default(),
        });
        assert_eq!(t.kind(), ObjectKind::Trigger);
        assert_eq!(t.object_ref().owner(), Some(table));
        let json = round_trip(&t);
        assert_eq!(json["timing"], "before");
        // `enabled` defaults to true when absent.
        let mut j = json.clone();
        j.as_object_mut().unwrap().remove("enabled");
        assert_eq!(serde_json::from_value::<ObjectDef>(j).unwrap(), t);

        let mut q = SequenceDef::new(schema.child(ObjectKind::Sequence, "m2_seq"));
        q.max_value = Some(i128::from(i64::MAX));
        q.min_value = Some(-5);
        q.cycle = true;
        let q = ObjectDef::Sequence(q);
        let json = round_trip(&q);
        assert_eq!(json["start"], "1");
        assert_eq!(json["max_value"], "9223372036854775807");
        assert_eq!(json["min_value"], "-5");
        assert!(json.get("cache").is_none());
        // Plain JSON numbers are accepted too.
        let mut j = json.clone();
        j["start"] = serde_json::json!(7);
        let ObjectDef::Sequence(back) = serde_json::from_value::<ObjectDef>(j).unwrap() else {
            panic!("not a sequence");
        };
        assert_eq!(back.start, 7);
    }

    #[test]
    fn identity_columns() {
        let mut c = ColumnDef::new("id", DataType::INT64).not_null();
        c.auto_increment = true;
        c.identity = Some(IdentityDef {
            always: true,
            start: Some(1),
            increment: Some(1),
        });
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["identity"]["start"], "1");
        assert_eq!(serde_json::from_value::<ColumnDef>(json).unwrap(), c);
        // Older JSON without the field still loads.
        let old: ColumnDef =
            serde_json::from_str(r#"{"name":"x","data_type":{"type":"bool"},"nullable":true}"#)
                .unwrap();
        assert_eq!(old.identity, None);
    }
}
