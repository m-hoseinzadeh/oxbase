//! Catalog [`Fetcher`] cells (M4, **fetch** stream). Contract: `docs/m4-contracts.md` §3 and the
//! [`crate::dialect`] module docs.
//!
//! Rules on top of the M2 `Fetcher` rules (bound parameters only, sorted output, `system`
//! flags, `Cancelled` with nothing partial, `NotFound` from `load`):
//!
//! - One session reads one database ([`oxbase_dialect::DatabaseScope::Session`]): queries use
//!   the session database's catalog views (`sys.schemas`, `sys.objects`, `sys.columns`, ...) and
//!   never a three-part name. A ref whose `catalog` is not
//!   [`Connection::current_database`](oxbase_driver::Connection::current_database) is
//!   `InvalidInput`. The `Database` listing reads `sys.databases` (server-wide).
//! - Names are bound as `@P1..@Pn` (`nvarchar`). Objects are found by joining `sys.objects` on
//!   the schema name and the object name, both bound; children are then read by object id
//!   (bound as `@P1`). No name is ever formatted into SQL text: every query is a `&'static str`
//!   in [`sql`](self::sql), and `rows::query` only accepts `&'static str`.
//! - System objects: databases `master`, `model`, `msdb`, `tempdb` (and `database_id <= 4`);
//!   schemas `sys`, `INFORMATION_SCHEMA`, `guest` and the fixed database-role schemas
//!   (`db_owner`, `db_datareader`, ..., `schema_id >= 16384`); objects with `is_ms_shipped = 1`
//!   (and their children). Listed with `system = true`.
//! - Definitions of views, routines and triggers come from `sys.sql_modules.definition`,
//!   unchanged; `NULL` (`WITH ENCRYPTION`) sets [`crate::extras::ENCRYPTED`]. A view's
//!   `definition` is the text after the header's `AS` (comments inside the body kept).
//! - Comments: the `MS_Description` extended property (`sys.extended_properties`, class 1 for
//!   objects and columns, 3 for schemas, 6 for types, 7 for indexes, 0 for the database).
//! - Defaults and check expressions are the server's normalized text (`((0))`, `([qty]>=(0))`);
//!   the comparator rules (ddl stream) fold the parentheses.
//! - Listings are sorted by name in binary (Rust string) order; columns by `column_id`.

mod rows;
mod sql;
mod text;
mod types;

use std::collections::BTreeMap;

use async_trait::async_trait;
use oxbase_dialect::{BracketQuoting, Fetcher, Quoting};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    ArgMode, CheckDef, ColumnDef, Error, Extras, FkAction, ForeignKeyDef, GeneratedColumn,
    GenericDef, IdentityDef, IndexColumn, IndexDef, ObjectDef, ObjectKind, ObjectRef,
    ObjectSummary, PrimaryKeyDef, Result, RoutineArg, RoutineDef, SchemaDef, SequenceDef,
    ServerVersion, TableDef, TriggerDef, TriggerEvent, TriggerLevel, TriggerTiming, Value, ViewDef,
    props,
};

use crate::extras as x;
use rows::{Row, Rows, query};
use types::TypeInfo;

/// The kinds with a fetcher: everything in the tree layout.
pub(crate) const KINDS: [ObjectKind; 13] = [
    ObjectKind::Database,
    ObjectKind::Schema,
    ObjectKind::Table,
    ObjectKind::Column,
    ObjectKind::Index,
    ObjectKind::ForeignKey,
    ObjectKind::Check,
    ObjectKind::View,
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Trigger,
    ObjectKind::Sequence,
    ObjectKind::Type,
];

/// The fetcher of one object kind.
pub(crate) struct MsSqlFetcher {
    kind: ObjectKind,
}

impl MsSqlFetcher {
    pub(crate) const fn new(kind: ObjectKind) -> Self {
        Self { kind }
    }
}

#[async_trait]
impl Fetcher for MsSqlFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let cx = Cx::new(c, &cancel)?;
        let mut out = match self.kind {
            ObjectKind::Database => list_databases(&cx, parent).await?,
            ObjectKind::Schema => list_schemas(&cx, parent).await?,
            ObjectKind::Table => list_tables(&cx, parent).await?,
            ObjectKind::View
            | ObjectKind::Function
            | ObjectKind::Procedure
            | ObjectKind::Sequence
            | ObjectKind::Type => list_schema_objects(&cx, self.kind, parent).await?,
            ObjectKind::Column
            | ObjectKind::Index
            | ObjectKind::ForeignKey
            | ObjectKind::Check
            | ObjectKind::Trigger => list_table_children(&cx, self.kind, parent).await?,
            other => return Err(unsupported(other)),
        };
        if self.kind != ObjectKind::Column {
            out.sort_by(|a, b| a.obj.name.cmp(&b.obj.name));
        }
        Ok(out)
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        let cx = Cx::new(c, &cancel)?;
        if obj.kind != self.kind {
            return Err(Error::invalid_input(format!(
                "the SQL Server {} fetcher cannot load {obj}",
                self.kind.display_name()
            )));
        }
        match self.kind {
            ObjectKind::Database => load_database(&cx, obj).await,
            ObjectKind::Schema => load_schema(&cx, obj).await,
            ObjectKind::Table => load_table(&cx, obj).await,
            ObjectKind::View => load_view(&cx, obj).await,
            ObjectKind::Function | ObjectKind::Procedure => load_routine(&cx, obj).await,
            ObjectKind::Sequence => load_sequence(&cx, obj).await,
            ObjectKind::Type => load_type(&cx, obj).await,
            ObjectKind::Column
            | ObjectKind::Index
            | ObjectKind::ForeignKey
            | ObjectKind::Check
            | ObjectKind::Trigger => load_table_child(&cx, obj).await,
            other => Err(unsupported(other)),
        }
    }
}

fn unsupported(kind: ObjectKind) -> Error {
    Error::unsupported(format!(
        "SQL Server has no fetcher for {}",
        kind.plural_display_name().to_lowercase()
    ))
}

fn not_found(obj: &ObjectRef) -> Error {
    Error::not_found(format!(
        "{} {} does not exist",
        obj.kind.display_name(),
        obj.path().join(".")
    ))
}

// ---------------------------------------------------------------------------------------------
// Context and parents

/// The connection and token of one fetcher call.
struct Cx<'a> {
    c: &'a dyn Connection,
    cancel: &'a CancellationToken,
}

impl<'a> Cx<'a> {
    fn new(c: &'a dyn Connection, cancel: &'a CancellationToken) -> Result<Self> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let v = c.server_version();
        if (1..14).contains(&v.major) {
            return Err(Error::unsupported(format!(
                "reading the SQL Server catalog needs SQL Server 2017 (14.x) or newer (server is \
                 {v})"
            )));
        }
        Ok(Self { c, cancel })
    }

    fn version(&self) -> &ServerVersion {
        self.c.server_version()
    }

    async fn query(&self, sql: &'static str, params: &[Value]) -> Result<Rows> {
        query(self.c, sql, params, self.cancel).await
    }

    /// Fails unless this session is on `database` (when both are known).
    fn check_database(&self, database: Option<&str>) -> Result<()> {
        if let (Some(want), Some(have)) = (database, self.c.current_database())
            && want != have
        {
            return Err(Error::invalid_input(format!(
                "this SQL Server session is on database [{have}]; open a session on [{want}] to \
                 read its objects"
            )));
        }
        Ok(())
    }

    /// `r` with `catalog` set to the session's database when it was missing.
    fn in_session_database(&self, r: &ObjectRef) -> Result<ObjectRef> {
        self.check_database(r.catalog.as_deref())?;
        let mut r = r.clone();
        if r.catalog.is_none() {
            r.catalog = self.c.current_database();
        }
        Ok(r)
    }
}

fn expect_parent(parent: &ObjectRef, kinds: &[ObjectKind], child: ObjectKind) -> Result<()> {
    if kinds.contains(&parent.kind) && !parent.is_root() && !parent.name.is_empty() {
        Ok(())
    } else {
        Err(Error::invalid_input(format!(
            "SQL Server {} are listed under a {}, not under {parent}",
            child.plural_display_name().to_lowercase(),
            kinds
                .iter()
                .map(|k| k.display_name().to_lowercase())
                .collect::<Vec<_>>()
                .join(" or ")
        )))
    }
}

/// `true` for the schemas SQL Server owns: `sys`, `INFORMATION_SCHEMA`, `guest` and the fixed
/// database-role schemas (`schema_id >= 16384`).
pub(crate) fn is_system_schema(name: &str, schema_id: Option<i64>) -> bool {
    matches!(name, "sys" | "INFORMATION_SCHEMA" | "guest")
        || schema_id.is_some_and(|id| id >= 16384)
        || (schema_id.is_none() && name.starts_with("db_"))
}

/// The schema of a schema-level object or table child.
fn schema_of(obj: &ObjectRef) -> Result<&str> {
    obj.schema
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::invalid_input(format!("{obj} has no schema")))
}

/// The object kind of a `sys.objects.type` code.
fn object_kind(code: &str) -> Option<ObjectKind> {
    match code {
        "U" => Some(ObjectKind::Table),
        "V" => Some(ObjectKind::View),
        "FN" | "IF" | "TF" | "FS" | "FT" | "AF" => Some(ObjectKind::Function),
        "P" | "PC" => Some(ObjectKind::Procedure),
        "SO" => Some(ObjectKind::Sequence),
        _ => None,
    }
}

/// [`x::FUNCTION_KIND`] of a function's type code.
fn function_kind(code: &str) -> Option<&'static str> {
    match code {
        "FN" => Some("scalar"),
        "IF" => Some("inline_table"),
        "TF" => Some("table"),
        "FS" => Some("clr_scalar"),
        "FT" => Some("clr_table"),
        "AF" => Some("aggregate"),
        _ => None,
    }
}

/// `true` for CLR modules (no `sys.sql_modules` row).
fn is_clr(code: &str) -> bool {
    matches!(code, "FS" | "FT" | "AF" | "PC")
}

/// A schema-scoped object found by schema and name.
struct Found {
    id: i64,
    /// The ref with the kind and name the catalog reports.
    obj: ObjectRef,
    shipped: bool,
}

/// Looks up the object `r` names (its `kind` may be a guess; it is corrected from the catalog).
async fn find_object(cx: &Cx<'_>, r: &ObjectRef) -> Result<Option<Found>> {
    let r = cx.in_session_database(r)?;
    let schema = schema_of(&r)?;
    let rows = cx
        .query(
            sql::OBJECT,
            &[Value::from(schema), Value::from(r.name.as_str())],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let Some(kind) = object_kind(row.code("type")?) else {
        return Ok(None);
    };
    let mut obj = r.clone();
    obj.kind = kind;
    obj.name = row.string("name")?;
    Ok(Some(Found {
        id: row.i64("object_id")?,
        obj,
        shipped: row.flag("is_ms_shipped")?,
    }))
}

// ---------------------------------------------------------------------------------------------
// Databases and schemas

async fn list_databases(cx: &Cx<'_>, parent: &ObjectRef) -> Result<Vec<ObjectSummary>> {
    if !parent.is_root() {
        return Err(Error::invalid_input(format!(
            "SQL Server databases are listed under the server root, not under {parent}"
        )));
    }
    let rows = cx.query(sql::DATABASES, &[]).await?;
    rows.iter()
        .map(|row| {
            let mut s = ObjectSummary::new(parent.child(ObjectKind::Database, row.string("name")?));
            s.system = row.i64("database_id")? <= 4;
            s.extras.insert(props::CURRENT, row.flag("is_current")?);
            if let Some(state) = row.opt_string("state_desc")? {
                s.extras.insert(x::STATE, state);
            }
            if let Some(coll) = row.opt_string("collation_name")? {
                s.extras.insert(props::COLLATION, coll);
            }
            Ok(s)
        })
        .collect()
}

async fn load_database(cx: &Cx<'_>, obj: &ObjectRef) -> Result<ObjectDef> {
    let rows = cx
        .query(sql::DATABASE, &[Value::from(obj.name.as_str())])
        .await?;
    let row = rows.first().ok_or_else(|| not_found(obj))?;
    let mut def = SchemaDef::new(ObjectRef::database(row.string("name")?));
    def.owner = row.opt_string("owner")?;
    def.comment = row.opt_string("comment")?;
    for (prop, column) in [
        (props::COLLATION, "collation_name"),
        (x::RECOVERY_MODEL, "recovery_model_desc"),
        (x::STATE, "state_desc"),
    ] {
        if let Some(v) = row.opt_string(column)? {
            def.extras.insert(prop, v);
        }
    }
    if let Some(level) = row.opt_i64("compatibility_level")? {
        def.extras.insert(x::COMPATIBILITY_LEVEL, level);
    }
    def.extras.insert(props::CURRENT, row.flag("is_current")?);
    Ok(ObjectDef::Schema(def))
}

async fn list_schemas(cx: &Cx<'_>, parent: &ObjectRef) -> Result<Vec<ObjectSummary>> {
    expect_parent(parent, &[ObjectKind::Database], ObjectKind::Schema)?;
    cx.check_database(Some(&parent.name))?;
    let rows = cx.query(sql::SCHEMAS, &[]).await?;
    rows.iter()
        .map(|row| {
            let name = row.string("name")?;
            let mut s = ObjectSummary::new(parent.child(ObjectKind::Schema, &name));
            s.system = is_system_schema(&name, row.opt_i64("schema_id")?);
            s.comment = row.opt_string("comment")?;
            Ok(s)
        })
        .collect()
}

async fn load_schema(cx: &Cx<'_>, obj: &ObjectRef) -> Result<ObjectDef> {
    let obj = cx.in_session_database(obj)?;
    let rows = cx
        .query(sql::SCHEMA, &[Value::from(obj.name.as_str())])
        .await?;
    let row = rows.first().ok_or_else(|| not_found(&obj))?;
    let mut def = SchemaDef::new(obj);
    def.obj.name = row.string("name")?;
    def.owner = row.opt_string("owner")?;
    def.comment = row.opt_string("comment")?;
    Ok(ObjectDef::Schema(def))
}

// ---------------------------------------------------------------------------------------------
// Schema-level objects

/// The normalized schema parent of a schema-level listing.
fn schema_parent(cx: &Cx<'_>, parent: &ObjectRef, kind: ObjectKind) -> Result<ObjectRef> {
    expect_parent(parent, &[ObjectKind::Schema], kind)?;
    cx.in_session_database(parent)
}

/// Table facts from a row of `sql::tables` / `sql::table_head`.
fn table_extras(row: Row<'_>, extras: &mut Extras, full: bool) -> Result<()> {
    match row.opt_i64("temporal_type")? {
        Some(1) => {
            extras.insert(x::TEMPORAL_TYPE, "history");
        }
        Some(2) => {
            extras.insert(x::TEMPORAL_TYPE, "system_versioned");
        }
        _ => {}
    }
    let table_type = if row.flag("is_node")? {
        Some("graph_node")
    } else if row.flag("is_edge")? {
        Some("graph_edge")
    } else if row.opt_i64("ledger_type")?.unwrap_or(0) != 0 {
        Some("ledger")
    } else {
        None
    };
    if let Some(t) = table_type {
        extras.insert(props::TABLE_TYPE, t);
    }
    if row.flag("is_memory_optimized")? {
        extras.insert(x::MEMORY_OPTIMIZED, true);
    }
    if full {
        if let (Some(schema), Some(name)) =
            (row.opt_str("history_schema")?, row.opt_str("history_name")?)
        {
            extras.insert(x::HISTORY_TABLE, format!("{schema}.{name}"));
        }
        if let Some(c) = row.opt_string("data_compression")?.filter(|c| c != "NONE") {
            extras.insert(x::DATA_COMPRESSION, c);
        }
    }
    Ok(())
}

async fn list_tables(cx: &Cx<'_>, parent: &ObjectRef) -> Result<Vec<ObjectSummary>> {
    let parent = schema_parent(cx, parent, ObjectKind::Table)?;
    let rows = cx
        .query(
            sql::tables(cx.version()),
            &[Value::from(parent.name.as_str())],
        )
        .await?;
    let system = is_system_schema(&parent.name, None);
    rows.iter()
        .map(|row| {
            let mut s = ObjectSummary::new(parent.child(ObjectKind::Table, row.string("name")?));
            s.system = system || row.flag("is_ms_shipped")?;
            s.comment = row.opt_string("comment")?;
            s.row_estimate = row
                .opt_i64("row_estimate")?
                .and_then(|n| u64::try_from(n).ok());
            table_extras(row, &mut s.extras, false)?;
            Ok(s)
        })
        .collect()
}

async fn list_schema_objects(
    cx: &Cx<'_>,
    kind: ObjectKind,
    parent: &ObjectRef,
) -> Result<Vec<ObjectSummary>> {
    let parent = schema_parent(cx, parent, kind)?;
    let sql = match kind {
        ObjectKind::View => sql::VIEWS,
        ObjectKind::Function => sql::FUNCTIONS,
        ObjectKind::Procedure => sql::PROCEDURES,
        ObjectKind::Sequence => sql::SEQUENCES,
        _ => sql::TYPES,
    };
    let rows = cx.query(sql, &[Value::from(parent.name.as_str())]).await?;
    let system = is_system_schema(&parent.name, None);
    rows.iter()
        .map(|row| {
            let mut s = ObjectSummary::new(parent.child(kind, row.string("name")?));
            s.comment = row.opt_string("comment")?;
            if kind == ObjectKind::Type {
                s.system = system;
                s.extras.insert(x::TYPE_KIND, type_kind(row)?);
            } else {
                s.system = system || row.flag("is_ms_shipped")?;
            }
            if kind == ObjectKind::Function
                && let Some(fk) = function_kind(row.code("type")?)
            {
                s.extras.insert(x::FUNCTION_KIND, fk);
            }
            Ok(s)
        })
        .collect()
}

fn type_kind(row: Row<'_>) -> Result<&'static str> {
    Ok(if row.flag("is_table_type")? {
        "table"
    } else if row.flag("is_assembly_type")? {
        "clr"
    } else {
        "alias"
    })
}

async fn load_table(cx: &Cx<'_>, obj: &ObjectRef) -> Result<ObjectDef> {
    let found = find_object(cx, obj)
        .await?
        .filter(|f| f.obj.kind == ObjectKind::Table)
        .ok_or_else(|| not_found(obj))?;
    let head = cx
        .query(sql::table_head(cx.version()), &[Value::Int(found.id)])
        .await?;
    let head = head.first().ok_or_else(|| not_found(obj))?;
    let mut def = TableDef::new(found.obj.clone());
    def.comment = head.opt_string("comment")?;
    table_extras(head, &mut def.extras, true)?;
    def.columns = columns(cx, found.id)
        .await?
        .into_iter()
        .map(|c| c.def)
        .collect();
    for index in indexes(cx, found.id).await? {
        if index.primary {
            def.extras.insert(props::CLUSTERED, index.clustered);
            def.primary_key = Some(PrimaryKeyDef {
                name: Some(index.def.name),
                columns: index.def.columns.into_iter().map(|c| c.name).collect(),
            });
        } else {
            def.indexes.push(index.def);
        }
    }
    def.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    def.foreign_keys = foreign_keys(cx, found.id, found.obj.catalog.as_deref())
        .await?
        .into_iter()
        .map(|(fk, _)| fk)
        .collect();
    def.checks = checks(cx, found.id)
        .await?
        .into_iter()
        .map(|(ck, _)| ck)
        .collect();
    Ok(ObjectDef::Table(def))
}

async fn load_view(cx: &Cx<'_>, obj: &ObjectRef) -> Result<ObjectDef> {
    let obj = cx.in_session_database(obj)?;
    let schema = schema_of(&obj)?;
    let rows = cx
        .query(
            sql::VIEW,
            &[Value::from(schema), Value::from(obj.name.as_str())],
        )
        .await?;
    let row = rows.first().ok_or_else(|| not_found(&obj))?;
    let mut extras = Extras::new();
    if row.flag("with_check_option")? {
        extras.insert(props::CHECK_OPTION, "cascaded");
    }
    extras.insert(x::SCHEMA_BINDING, row.flag("is_schema_bound")?);
    let (definition, columns) = match row.opt_str("definition")? {
        Some(text) => match text::split_view(text) {
            Some(v) => (v.query, v.columns),
            None => (text.trim().to_owned(), Vec::new()),
        },
        None => {
            extras.insert(x::ENCRYPTED, true);
            (String::new(), Vec::new())
        }
    };
    let name = row.string("name")?;
    let comment = row.opt_string("comment")?;
    let mut obj = obj;
    obj.name = name;
    Ok(ObjectDef::View(ViewDef {
        obj,
        definition,
        columns,
        comment,
        extras,
    }))
}

async fn load_routine(cx: &Cx<'_>, obj: &ObjectRef) -> Result<ObjectDef> {
    let obj = cx.in_session_database(obj)?;
    let schema = schema_of(&obj)?;
    let rows = cx
        .query(
            sql::ROUTINE,
            &[Value::from(schema), Value::from(obj.name.as_str())],
        )
        .await?;
    let row = rows
        .first()
        .filter(|row| object_kind(row.code("type").unwrap_or("")) == Some(obj.kind))
        .ok_or_else(|| not_found(&obj))?;
    let code = row.code("type")?;
    let id = row.i64("object_id")?;
    let mut r = obj.clone();
    r.name = row.string("name")?;
    r.signature = None;
    let mut def = RoutineDef::new(r);
    def.language = Some((if is_clr(code) { "CLR" } else { "SQL" }).to_owned());
    def.definition = row.opt_string("definition")?;
    def.comment = row.opt_string("comment")?;
    if let Some(kind) = function_kind(code) {
        def.extras.insert(x::FUNCTION_KIND, kind);
    }
    if !is_clr(code) {
        def.extras
            .insert(x::SCHEMA_BINDING, row.flag("is_schema_bound")?);
        if def.definition.is_none() {
            def.extras.insert(x::ENCRYPTED, true);
        }
    }
    if let Some(principal) = row.opt_string("execute_as")? {
        def.extras.insert(x::EXECUTE_AS, principal);
    }
    if matches!(code, "IF" | "TF" | "FT") {
        def.returns = Some("TABLE".to_owned());
    }

    let params = cx.query(sql::PARAMETERS, &[Value::Int(id)]).await?;
    let mut readonly = Vec::new();
    for p in params.iter() {
        let ty = TypeInfo::from_row(p)?;
        if p.i64("parameter_id")? == 0 {
            if obj.kind == ObjectKind::Function && def.returns.is_none() {
                def.returns = Some(ty.native());
            }
            continue;
        }
        let name = p.string("name")?;
        if p.flag("is_readonly")? {
            readonly.push(serde_json::Value::from(name.clone()));
        }
        def.args.push(RoutineArg {
            name: Some(name),
            mode: if p.flag("is_output")? {
                ArgMode::InOut
            } else {
                ArgMode::In
            },
            native_type: ty.native(),
            default: None,
        });
    }
    if !readonly.is_empty() {
        def.extras.insert(
            x::READONLY_ARGS,
            Value::Json(serde_json::Value::from(readonly)),
        );
    }
    Ok(ObjectDef::Routine(def))
}

async fn load_sequence(cx: &Cx<'_>, obj: &ObjectRef) -> Result<ObjectDef> {
    let obj = cx.in_session_database(obj)?;
    let schema = schema_of(&obj)?;
    let rows = cx
        .query(
            sql::SEQUENCE,
            &[Value::from(schema), Value::from(obj.name.as_str())],
        )
        .await?;
    let row = rows.first().ok_or_else(|| not_found(&obj))?;
    let mut r = obj.clone();
    r.name = row.string("name")?;
    let mut def = SequenceDef::new(r);
    let data_type = TypeInfo::from_row(row)?.native();
    def.extras.insert(props::DATA_TYPE, data_type.clone());
    def.data_type = Some(data_type);
    def.start = row.opt_i128("start_value")?.unwrap_or(1);
    def.increment = row.opt_i128("increment")?.unwrap_or(1);
    def.min_value = row.opt_i128("minimum_value")?;
    def.max_value = row.opt_i128("maximum_value")?;
    def.cache = if row.flag("is_cached")? {
        row.opt_i64("cache_size")?.map(i128::from)
    } else {
        None
    };
    def.cycle = row.flag("is_cycling")?;
    def.comment = row.opt_string("comment")?;
    Ok(ObjectDef::Sequence(def))
}

async fn load_type(cx: &Cx<'_>, obj: &ObjectRef) -> Result<ObjectDef> {
    let obj = cx.in_session_database(obj)?;
    let schema = schema_of(&obj)?.to_owned();
    let rows = cx
        .query(
            sql::TYPE,
            &[Value::from(schema.as_str()), Value::from(obj.name.as_str())],
        )
        .await?;
    let row = rows.first().ok_or_else(|| not_found(&obj))?;
    let mut r = obj.clone();
    r.name = row.string("name")?;
    let q = BracketQuoting;
    let qualified = format!("{}.{}", q.quote_ident(&schema), q.quote_ident(&r.name));
    let kind = type_kind(row)?;
    let mut extras = Extras::new();
    extras.insert(x::TYPE_KIND, kind);
    let definition = match kind {
        "alias" => {
            let base = types::system_spelling(
                row.opt_str("base_type")?.unwrap_or_default(),
                row.opt_i64("max_length")?.unwrap_or(0),
                row.opt_i64("num_precision")?.unwrap_or(0),
                row.opt_i64("scale")?.unwrap_or(0),
            );
            let not_null = !row.flag("is_nullable")?;
            extras.insert(x::BASE_TYPE, base.clone());
            extras.insert(x::NOT_NULL, not_null);
            Some(format!(
                "CREATE TYPE {qualified} FROM {base}{}",
                if not_null { " NOT NULL" } else { " NULL" }
            ))
        }
        "table" => {
            let table_id = row
                .opt_i64("type_table_object_id")?
                .ok_or_else(|| not_found(&obj))?;
            Some(table_type_definition(cx, &qualified, table_id).await?)
        }
        _ => None,
    };
    Ok(ObjectDef::Generic(GenericDef {
        obj: r,
        definition,
        comment: row.opt_string("comment")?,
        extras,
    }))
}

/// `CREATE TYPE ... AS TABLE (...)` from the catalog of a table type's table.
async fn table_type_definition(cx: &Cx<'_>, qualified: &str, table_id: i64) -> Result<String> {
    let q = BracketQuoting;
    let mut parts = Vec::new();
    for col in columns(cx, table_id).await? {
        let c = &col.def;
        let mut s = q.quote_ident(&c.name);
        if let Some(g) = &c.generated {
            s.push_str(" AS ");
            s.push_str(&g.expression);
            if g.stored {
                s.push_str(" PERSISTED");
            }
        } else {
            s.push(' ');
            s.push_str(&col.type_ddl);
            if let Some(coll) = &c.collation {
                s.push_str(" COLLATE ");
                s.push_str(coll);
            }
            if let Some(id) = &c.identity {
                s.push_str(&format!(
                    " IDENTITY({}, {})",
                    id.start.unwrap_or(1),
                    id.increment.unwrap_or(1)
                ));
            }
            s.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
            if let Some(d) = &c.default {
                s.push_str(" DEFAULT ");
                s.push_str(d);
            }
        }
        parts.push(s);
    }
    let key_list = |cols: &[IndexColumn]| {
        cols.iter()
            .map(|k| {
                format!(
                    "{} {}",
                    q.quote_ident(&k.name),
                    if k.descending { "DESC" } else { "ASC" }
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    for index in indexes(cx, table_id).await? {
        let clustering = if index.clustered {
            "CLUSTERED"
        } else {
            "NONCLUSTERED"
        };
        let cols = key_list(&index.def.columns);
        parts.push(if index.primary {
            format!("PRIMARY KEY {clustering} ({cols})")
        } else if index.def.is_constraint {
            format!("UNIQUE {clustering} ({cols})")
        } else {
            format!(
                "INDEX {}{} {clustering} ({cols})",
                q.quote_ident(&index.def.name),
                if index.def.unique { " UNIQUE" } else { "" }
            )
        });
    }
    for (check, _) in checks(cx, table_id).await? {
        parts.push(format!("CHECK {}", check.expression));
    }
    Ok(format!(
        "CREATE TYPE {qualified} AS TABLE (\n    {}\n)",
        parts.join(",\n    ")
    ))
}

// ---------------------------------------------------------------------------------------------
// Table children

/// The owner (table or view) of a child listing or a child ref.
async fn find_owner(cx: &Cx<'_>, owner: &ObjectRef, missing: &ObjectRef) -> Result<Found> {
    find_object(cx, owner)
        .await?
        .filter(|f| matches!(f.obj.kind, ObjectKind::Table | ObjectKind::View))
        .ok_or_else(|| not_found(missing))
}

async fn list_table_children(
    cx: &Cx<'_>,
    kind: ObjectKind,
    parent: &ObjectRef,
) -> Result<Vec<ObjectSummary>> {
    expect_parent(parent, &[ObjectKind::Table, ObjectKind::View], kind)?;
    let parent = cx.in_session_database(parent)?;
    let owner = find_owner(cx, &parent, &parent).await?;
    let system = owner.shipped || is_system_schema(schema_of(&parent)?, None);
    // Refs keep the parent as the caller addressed it (the tree's node).
    let summary = |name: &str, comment: Option<String>| {
        let mut s = ObjectSummary::new(parent.child(kind, name));
        s.system = system;
        s.comment = comment;
        s
    };
    let out = match kind {
        ObjectKind::Column => columns(cx, owner.id)
            .await?
            .into_iter()
            .map(|c| {
                let mut s = summary(&c.def.name, c.def.comment.clone());
                s.extras
                    .insert(props::NATIVE_TYPE, c.def.native_type.clone());
                s.extras.insert(props::PRIMARY_KEY, c.in_primary_key);
                s
            })
            .collect(),
        ObjectKind::Index => indexes(cx, owner.id)
            .await?
            .into_iter()
            .map(|i| {
                let mut s = summary(&i.def.name, i.comment);
                s.extras.insert(props::PRIMARY_KEY, i.primary);
                s.extras.insert(props::UNIQUE, i.def.unique);
                s.extras.insert(props::CLUSTERED, i.clustered);
                if let Some(k) = i.def.extras.get(&x::INDEX_KIND) {
                    s.extras.insert(x::INDEX_KIND, k.clone());
                }
                s
            })
            .collect(),
        ObjectKind::ForeignKey => foreign_keys(cx, owner.id, parent.catalog.as_deref())
            .await?
            .into_iter()
            .map(|(fk, comment)| summary(fk.name.as_deref().unwrap_or_default(), comment))
            .collect(),
        ObjectKind::Check => checks(cx, owner.id)
            .await?
            .into_iter()
            .map(|(ck, comment)| summary(ck.name.as_deref().unwrap_or_default(), comment))
            .collect(),
        _ => triggers(cx, owner.id, &parent)
            .await?
            .into_iter()
            .map(|t| summary(&t.obj.name, t.comment))
            .collect(),
    };
    Ok(out)
}

async fn load_table_child(cx: &Cx<'_>, obj: &ObjectRef) -> Result<ObjectDef> {
    let obj = cx.in_session_database(obj)?;
    let owner_ref = obj
        .owner()
        .filter(|o| !o.name.is_empty())
        .ok_or_else(|| Error::invalid_input(format!("{obj} has no owning table")))?;
    let owner = find_owner(cx, &owner_ref, &obj).await?;
    let table = owner.obj.clone();
    match obj.kind {
        ObjectKind::Column => {
            let column = columns(cx, owner.id)
                .await?
                .into_iter()
                .find(|c| c.def.name == obj.name)
                .ok_or_else(|| not_found(&obj))?;
            Ok(ObjectDef::Column {
                table,
                column: column.def,
            })
        }
        ObjectKind::Index => {
            let index = indexes(cx, owner.id)
                .await?
                .into_iter()
                .find(|i| i.def.name == obj.name)
                .ok_or_else(|| not_found(&obj))?;
            let mut def = index.def;
            if index.primary {
                def.extras.insert(props::PRIMARY_KEY, true);
            }
            Ok(ObjectDef::Index { table, index: def })
        }
        ObjectKind::ForeignKey => {
            let (foreign_key, _) = foreign_keys(cx, owner.id, table.catalog.as_deref())
                .await?
                .into_iter()
                .find(|(fk, _)| fk.name.as_deref() == Some(obj.name.as_str()))
                .ok_or_else(|| not_found(&obj))?;
            Ok(ObjectDef::ForeignKey { table, foreign_key })
        }
        ObjectKind::Check => {
            let (check, _) = checks(cx, owner.id)
                .await?
                .into_iter()
                .find(|(ck, _)| ck.name.as_deref() == Some(obj.name.as_str()))
                .ok_or_else(|| not_found(&obj))?;
            Ok(ObjectDef::Check { table, check })
        }
        _ => {
            let trigger = triggers(cx, owner.id, &table)
                .await?
                .into_iter()
                .find(|t| t.obj.name == obj.name)
                .ok_or_else(|| not_found(&obj))?;
            Ok(ObjectDef::Trigger(trigger))
        }
    }
}

/// One column with listing facts.
struct Column {
    def: ColumnDef,
    in_primary_key: bool,
    /// The type as a `CREATE TYPE ... AS TABLE` column writes it (user types bracket-quoted).
    type_ddl: String,
}

async fn columns(cx: &Cx<'_>, id: i64) -> Result<Vec<Column>> {
    let rows = cx.query(sql::COLUMNS, &[Value::Int(id)]).await?;
    rows.iter().map(column).collect()
}

fn column(row: Row<'_>) -> Result<Column> {
    let ty = TypeInfo::from_row(row)?;
    let mut def = ColumnDef::new(row.string("name")?, ty.data_type());
    def.native_type = ty.native();
    def.nullable = row.flag("is_nullable")?;
    if let Some(default) = row.opt_string("default_definition")? {
        def.default = Some(default);
        if let Some(name) = row.opt_string("default_name")? {
            def.extras.insert(x::DEFAULT_CONSTRAINT, name);
        }
    }
    if row.flag("is_identity")? {
        def.auto_increment = true;
        def.identity = Some(IdentityDef {
            always: true,
            start: row.opt_i128("identity_seed")?,
            increment: row.opt_i128("identity_increment")?,
        });
        if row.flag("identity_not_for_replication")? {
            def.extras.insert(x::NOT_FOR_REPLICATION, true);
        }
    }
    if row.flag("is_computed")? {
        def.generated = Some(GeneratedColumn {
            expression: row.opt_string("computed_definition")?.unwrap_or_default(),
            stored: row.flag("is_persisted")?,
        });
    }
    if !ty.user_defined && ty.name == "timestamp" {
        def.extras.insert(x::ROW_VERSION, true);
    }
    match row.opt_i64("generated_always_type")? {
        Some(1) => {
            def.extras.insert(x::PERIOD, "row_start");
        }
        Some(2) => {
            def.extras.insert(x::PERIOD, "row_end");
        }
        _ => {}
    }
    if row.flag("is_hidden")? {
        def.extras.insert(x::HIDDEN, true);
    }
    if row.flag("is_sparse")? {
        def.extras.insert(x::SPARSE, true);
    }
    def.collation = row.opt_string("collation_name")?;
    def.comment = row.opt_string("comment")?;
    let q = BracketQuoting;
    let type_ddl = if ty.user_defined {
        format!("{}.{}", q.quote_ident(&ty.schema), q.quote_ident(&ty.name))
    } else {
        def.native_type.clone()
    };
    Ok(Column {
        def,
        in_primary_key: row.flag("in_primary_key")?,
        type_ddl,
    })
}

/// One index with listing facts.
struct Index {
    def: IndexDef,
    primary: bool,
    clustered: bool,
    comment: Option<String>,
}

/// [`x::INDEX_KIND`] of `sys.indexes.type`.
fn index_kind(code: i64) -> &'static str {
    match code {
        1 => "clustered",
        3 => "xml",
        4 => "spatial",
        5 => "clustered_columnstore",
        6 => "nonclustered_columnstore",
        7 => "hash",
        _ => "nonclustered",
    }
}

/// One `sys.index_columns` row.
struct IndexPart {
    name: String,
    key_ordinal: i64,
    descending: bool,
    included: bool,
}

/// The indexes (the primary key's included) of `id`, in name order.
async fn indexes(cx: &Cx<'_>, id: i64) -> Result<Vec<Index>> {
    let rows = cx.query(sql::INDEXES, &[Value::Int(id)]).await?;
    let col_rows = cx.query(sql::INDEX_COLUMNS, &[Value::Int(id)]).await?;
    let mut parts: BTreeMap<i64, Vec<IndexPart>> = BTreeMap::new();
    for row in col_rows.iter() {
        parts
            .entry(row.i64("index_id")?)
            .or_default()
            .push(IndexPart {
                name: row.string("name")?,
                key_ordinal: row.i64("key_ordinal")?,
                descending: row.flag("is_descending_key")?,
                included: row.flag("is_included_column")?,
            });
    }
    let mut out = Vec::new();
    for row in rows.iter() {
        let index_id = row.i64("index_id")?;
        let code = row.i64("type")?;
        let parts = parts.remove(&index_id).unwrap_or_default();
        // The query orders by key ordinal (0 for non-key columns) then column id.
        let mut keys: Vec<IndexColumn> = parts
            .iter()
            .filter(|p| p.key_ordinal > 0)
            .map(|p| IndexColumn {
                descending: p.descending,
                ..IndexColumn::column(&p.name)
            })
            .collect();
        if code == 6 && keys.is_empty() {
            // A nonclustered columnstore index lists its columns without key ordinals.
            keys = parts
                .iter()
                .filter(|p| !p.included)
                .map(|p| IndexColumn::column(&p.name))
                .collect();
        }
        let include: Vec<serde_json::Value> = parts
            .iter()
            .filter(|p| p.included && p.key_ordinal == 0)
            .map(|p| serde_json::Value::from(p.name.clone()))
            .collect();
        let clustered = matches!(code, 1 | 5);
        let mut extras = Extras::new();
        extras.insert(props::CLUSTERED, clustered);
        extras.insert(x::INDEX_KIND, index_kind(code));
        extras.insert(
            props::INCLUDE_COLUMNS,
            Value::Json(serde_json::Value::from(include)),
        );
        if let Some(ff) = row.opt_i64("fill_factor")?.filter(|f| *f != 0) {
            extras.insert(props::FILL_FACTOR, ff);
        }
        extras.insert(x::DISABLED, row.flag("is_disabled")?);
        extras.insert(x::IGNORE_DUP_KEY, row.flag("ignore_dup_key")?);
        if let Some(c) = row.opt_string("data_compression")?.filter(|c| c != "NONE") {
            extras.insert(x::DATA_COMPRESSION, c);
        }
        let primary = row.flag("is_primary_key")?;
        out.push(Index {
            def: IndexDef {
                name: row.opt_string("name")?.unwrap_or_default(),
                columns: keys,
                unique: row.flag("is_unique")?,
                is_constraint: row.flag("is_unique_constraint")?,
                predicate: row.opt_string("filter_definition")?,
                extras,
            },
            primary,
            clustered,
            comment: row.opt_string("comment")?,
        });
    }
    out.sort_by(|a, b| a.def.name.cmp(&b.def.name));
    Ok(out)
}

fn fk_action(code: i64) -> FkAction {
    match code {
        1 => FkAction::Cascade,
        2 => FkAction::SetNull,
        3 => FkAction::SetDefault,
        _ => FkAction::NoAction,
    }
}

/// The foreign keys of table `id` with their comments, in name order. `catalog` is the
/// database of the referenced tables (always the table's own).
async fn foreign_keys(
    cx: &Cx<'_>,
    id: i64,
    catalog: Option<&str>,
) -> Result<Vec<(ForeignKeyDef, Option<String>)>> {
    let rows = cx.query(sql::FOREIGN_KEYS, &[Value::Int(id)]).await?;
    let mut out: Vec<(i64, ForeignKeyDef, Option<String>)> = Vec::new();
    for row in rows.iter() {
        let fk_id = row.i64("fk_id")?;
        if out.last().is_none_or(|(last, _, _)| *last != fk_id) {
            let mut extras = Extras::new();
            extras.insert(x::NOT_TRUSTED, row.flag("is_not_trusted")?);
            extras.insert(x::NOT_FOR_REPLICATION, row.flag("is_not_for_replication")?);
            extras.insert(x::DISABLED, row.flag("is_disabled")?);
            let ref_schema = row.string("ref_schema")?;
            out.push((
                fk_id,
                ForeignKeyDef {
                    name: Some(row.string("name")?),
                    columns: Vec::new(),
                    ref_table: ObjectRef::table(
                        catalog,
                        Some(ref_schema.as_str()),
                        row.string("ref_table")?,
                    ),
                    ref_columns: Vec::new(),
                    on_delete: fk_action(row.i64("on_delete")?),
                    on_update: fk_action(row.i64("on_update")?),
                    extras,
                },
                row.opt_string("comment")?,
            ));
        }
        if let Some((_, fk, _)) = out.last_mut() {
            fk.columns.push(row.string("column_name")?);
            fk.ref_columns.push(row.string("ref_column_name")?);
        }
    }
    let mut out: Vec<_> = out.into_iter().map(|(_, fk, c)| (fk, c)).collect();
    out.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    Ok(out)
}

/// The check constraints of `id` with their comments, in name order.
async fn checks(cx: &Cx<'_>, id: i64) -> Result<Vec<(CheckDef, Option<String>)>> {
    let rows = cx.query(sql::CHECKS, &[Value::Int(id)]).await?;
    let mut out = rows
        .iter()
        .map(|row| {
            let mut extras = Extras::new();
            extras.insert(x::NOT_TRUSTED, row.flag("is_not_trusted")?);
            extras.insert(x::DISABLED, row.flag("is_disabled")?);
            extras.insert(x::NOT_FOR_REPLICATION, row.flag("is_not_for_replication")?);
            Ok((
                CheckDef {
                    name: Some(row.string("name")?),
                    expression: row.opt_string("definition")?.unwrap_or_default(),
                    extras,
                },
                row.opt_string("comment")?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    out.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    Ok(out)
}

/// The DML triggers of table or view `id` (refs under `owner`), in name order.
async fn triggers(cx: &Cx<'_>, id: i64, owner: &ObjectRef) -> Result<Vec<TriggerDef>> {
    let rows = cx.query(sql::TRIGGERS, &[Value::Int(id)]).await?;
    let mut out = rows
        .iter()
        .map(|row| {
            let mut events = Vec::new();
            for (column, event) in [
                ("on_insert", TriggerEvent::Insert),
                ("on_update", TriggerEvent::Update),
                ("on_delete", TriggerEvent::Delete),
            ] {
                if row.flag(column)? {
                    events.push(event);
                }
            }
            let definition = row.opt_string("definition")?;
            let mut extras = Extras::new();
            if definition.is_none() {
                extras.insert(x::ENCRYPTED, true);
            }
            if let Some(principal) = row.opt_string("execute_as")? {
                extras.insert(x::EXECUTE_AS, principal);
            }
            extras.insert(x::NOT_FOR_REPLICATION, row.flag("is_not_for_replication")?);
            Ok(TriggerDef {
                obj: owner.child(ObjectKind::Trigger, row.string("name")?),
                timing: if row.flag("is_instead_of_trigger")? {
                    TriggerTiming::InsteadOf
                } else {
                    TriggerTiming::After
                },
                events,
                update_columns: Vec::new(),
                level: TriggerLevel::Statement,
                condition: None,
                function: None,
                body: None,
                definition,
                enabled: !row.flag("is_disabled")?,
                comment: row.opt_string("comment")?,
                extras,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    out.sort_by(|a, b| a.obj.name.cmp(&b.obj.name));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_schemas() {
        assert!(is_system_schema("sys", Some(4)));
        assert!(is_system_schema("INFORMATION_SCHEMA", Some(3)));
        assert!(is_system_schema("guest", Some(2)));
        assert!(is_system_schema("db_owner", Some(16384)));
        assert!(is_system_schema("db_datareader", None));
        assert!(!is_system_schema("dbo", Some(1)));
        assert!(!is_system_schema("m2", Some(5)));
        assert!(!is_system_schema("db_custom", Some(7)));
    }

    #[test]
    fn object_codes() {
        assert_eq!(object_kind("U"), Some(ObjectKind::Table));
        assert_eq!(object_kind("V"), Some(ObjectKind::View));
        assert_eq!(object_kind("IF"), Some(ObjectKind::Function));
        assert_eq!(object_kind("PC"), Some(ObjectKind::Procedure));
        assert_eq!(object_kind("SO"), Some(ObjectKind::Sequence));
        assert_eq!(object_kind("TR"), None);
        assert_eq!(function_kind("TF"), Some("table"));
        assert_eq!(function_kind("IF"), Some("inline_table"));
        assert_eq!(function_kind("P"), None);
        assert!(is_clr("FS"));
        assert!(!is_clr("FN"));
        assert_eq!(index_kind(1), "clustered");
        assert_eq!(index_kind(2), "nonclustered");
        assert_eq!(index_kind(6), "nonclustered_columnstore");
        assert_eq!(fk_action(1), FkAction::Cascade);
        assert_eq!(fk_action(3), FkAction::SetDefault);
        assert_eq!(fk_action(0), FkAction::NoAction);
    }

    #[test]
    fn every_kind_has_a_listing() {
        for kind in KINDS {
            assert!(
                !matches!(
                    kind,
                    ObjectKind::MatView | ObjectKind::Event | ObjectKind::Extension
                ),
                "{kind}"
            );
        }
    }
}
