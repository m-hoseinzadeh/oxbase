//! The PostgreSQL [`Fetcher`] cells: one [`PgFetcher`] per object kind.
//!
//! Parents follow the M2 convention (`Fetcher` docs): databases under the root, schemas under a
//! database, schema-level kinds under a schema, table children under their table, view or
//! materialized view. A session sees one database ([`DatabaseScope::Session`]): a request for an
//! object of another database fails with `InvalidInput` instead of reading the wrong catalog.
//! Databases themselves (and their `load`) are server-wide and work from any session.
//!
//! [`DatabaseScope::Session`]: oxbase_dialect::DatabaseScope::Session

use std::collections::BTreeMap;

use async_trait::async_trait;
use oxbase_dialect::Fetcher;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    ArgMode, CheckDef, ColumnDef, Error, Extras, FkAction, ForeignKeyDef, GeneratedColumn,
    GenericDef, IdentityDef, IndexColumn, IndexDef, ObjectDef, ObjectKind, ObjectRef,
    ObjectSummary, PrimaryKeyDef, PropId, Result, RoutineArg, RoutineDef, SchemaDef, SequenceDef,
    ServerVersion, TableDef, TriggerDef, TriggerEvent, TriggerLevel, TriggerTiming, Value, ViewDef,
    props,
};

use super::rows::{Row, Rows, query};
use super::{sql, types};

/// Extra: an index treats NULLs as equal (`NULLS NOT DISTINCT`, PostgreSQL 15+).
pub const NULLS_NOT_DISTINCT: PropId = PropId::from_static("nulls_not_distinct");
/// Extra: a constraint is `NOT VALID` (not yet checked against existing rows).
pub const NOT_VALID: PropId = PropId::from_static("not_valid");
/// Extra: a check constraint is `NO INHERIT`.
pub const NO_INHERIT: PropId = PropId::from_static("no_inherit");
/// Extra: a foreign key's `MATCH` type (`full`, `partial`; absent for `simple`).
pub const MATCH_TYPE: PropId = PropId::from_static("match_type");
/// Extra: a routine's parallel safety (`safe`, `restricted`, `unsafe`).
pub const PARALLEL: PropId = PropId::from_static("parallel");
/// Extra: database `LC_CTYPE`.
pub const CTYPE: PropId = PropId::from_static("ctype");
/// Extra: the kind of a user-defined type (`enum`, `composite`, `domain`, `range`,
/// `multirange`).
pub const TYPE_KIND: PropId = PropId::from_static("type_kind");
/// Extra: the base type of a domain.
pub const BASE_TYPE: PropId = PropId::from_static("base_type");
/// Extra: the labels of an enum, as SQL literals (`'a', 'b'`).
pub const ENUM_LABELS: PropId = PropId::from_static("enum_labels");
/// Extra: an extension's version.
pub const EXTENSION_VERSION: PropId = PropId::from_static("extension_version");
/// Extra: the schema an extension's objects live in.
pub const EXTENSION_SCHEMA: PropId = PropId::from_static("extension_schema");
/// Extra: a sequence belongs to an identity column (`GENERATED ... AS IDENTITY`).
pub const IDENTITY_SEQUENCE: PropId = PropId::from_static("identity_sequence");
/// Extra: a foreign key or check constraint is `NOT ENFORCED` (PostgreSQL 18+).
pub const NOT_ENFORCED: PropId = PropId::from_static("not_enforced");
/// Extra: the `EXCLUDE` constraint an index implements, as `pg_get_constraintdef` prints it
/// (`EXCLUDE USING gist (r WITH &&)`); such an index has `is_constraint` set.
pub const EXCLUSION: PropId = PropId::from_static("exclusion");
/// Extra: per key column of an index, whether NULLs sort first (a JSON array of booleans in
/// column order). Only set when a column differs from the default (`NULLS LAST` for ascending,
/// `NULLS FIRST` for descending columns).
pub const NULLS_FIRST: PropId = PropId::from_static("nulls_first");

/// Every kind with a PostgreSQL fetcher.
pub(crate) const KINDS: [ObjectKind; 15] = [
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
    ObjectKind::Type,
    ObjectKind::Extension,
];

/// Reads objects of one kind from a PostgreSQL session.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PgFetcher {
    kind: ObjectKind,
}

impl PgFetcher {
    pub(crate) const fn new(kind: ObjectKind) -> Self {
        Self { kind }
    }
}

#[async_trait]
impl Fetcher for PgFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let cx = Cx::new(c, &cancel)?;
        match self.kind {
            ObjectKind::Database => list_databases(&cx, parent).await,
            ObjectKind::Schema => list_schemas(&cx, parent).await,
            ObjectKind::Extension => list_extensions(&cx, parent).await,
            ObjectKind::Table | ObjectKind::View | ObjectKind::MatView | ObjectKind::Sequence => {
                list_relations(&cx, self.kind, parent).await
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                list_routines(&cx, self.kind, parent).await
            }
            ObjectKind::Type => list_types(&cx, parent).await,
            ObjectKind::Column
            | ObjectKind::Index
            | ObjectKind::ForeignKey
            | ObjectKind::Check
            | ObjectKind::Trigger => list_table_children(&cx, self.kind, parent).await,
            other => Err(unsupported(other)),
        }
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
                "the PostgreSQL {} fetcher cannot load {obj}",
                self.kind.display_name()
            )));
        }
        match self.kind {
            ObjectKind::Database => load_database(&cx, obj).await,
            ObjectKind::Schema => load_schema(&cx, obj).await,
            ObjectKind::Extension => load_extension(&cx, obj).await,
            ObjectKind::Table => load_table(&cx, obj).await,
            ObjectKind::View | ObjectKind::MatView => load_view(&cx, obj).await,
            ObjectKind::Sequence => load_sequence(&cx, obj).await,
            ObjectKind::Function | ObjectKind::Procedure => load_routine(&cx, obj).await,
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
        "PostgreSQL has no fetcher for {}",
        kind.display_name()
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
        if (1..10).contains(&v.major) {
            return Err(Error::unsupported(format!(
                "reading the PostgreSQL catalog needs PostgreSQL 10 or newer (server is {v})"
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
                "this PostgreSQL session is on database \"{have}\"; objects of \"{want}\" must \
                 be read through a session on that database"
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
            "PostgreSQL {} are listed under a {}, not under {parent}",
            child.plural_display_name().to_lowercase(),
            kinds
                .iter()
                .map(|k| k.display_name().to_lowercase())
                .collect::<Vec<_>>()
                .join(" or ")
        )))
    }
}

/// `true` for the schemas PostgreSQL owns.
pub(crate) fn is_system_schema(name: &str) -> bool {
    name == "pg_catalog" || name == "information_schema" || name.starts_with("pg_toast")
}

/// The schema of a schema-level object or table child.
fn schema_of(obj: &ObjectRef) -> Result<&str> {
    obj.schema
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::invalid_input(format!("{obj} has no schema")))
}

/// The object kind of a relation from its `relkind`.
fn relation_kind(relkind: &str) -> Option<ObjectKind> {
    match relkind {
        "r" | "p" | "f" => Some(ObjectKind::Table),
        "v" => Some(ObjectKind::View),
        "m" => Some(ObjectKind::MatView),
        "S" => Some(ObjectKind::Sequence),
        _ => None,
    }
}

/// A relation found by schema and name.
struct Relation {
    oid: i64,
    /// The owner ref, with the kind the catalog reports.
    obj: ObjectRef,
    /// An extension owns the relation (`pg_depend` type `e`).
    extension_member: bool,
}

/// Looks up the table, view or materialized view `owner` (a ref whose `kind` may be a guess;
/// it is corrected from the catalog). `None` when it does not exist.
async fn find_relation(cx: &Cx<'_>, owner: &ObjectRef) -> Result<Option<Relation>> {
    let owner = cx.in_session_database(owner)?;
    let schema = schema_of(&owner)?;
    let rows = cx
        .query(
            sql::RELATION,
            &[Value::from(schema), Value::from(owner.name.as_str())],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let Some(kind) = relation_kind(row.code("relkind")?) else {
        return Ok(None);
    };
    let mut obj = owner.clone();
    obj.kind = kind;
    Ok(Some(Relation {
        oid: row.i64("oid")?,
        obj,
        extension_member: row.flag("extension_member")?,
    }))
}

// ---------------------------------------------------------------------------------------------
// Databases, schemas, extensions

async fn list_databases(cx: &Cx<'_>, parent: &ObjectRef) -> Result<Vec<ObjectSummary>> {
    if !parent.is_root() {
        return Err(Error::invalid_input(format!(
            "PostgreSQL databases are listed under the server root, not under {parent}"
        )));
    }
    let rows = cx.query(sql::DATABASES, &[]).await?;
    rows.iter()
        .map(|row| {
            let mut s = ObjectSummary::new(parent.child(ObjectKind::Database, row.string("name")?));
            s.comment = row.opt_string("comment")?;
            if let Some(enc) = row.opt_string("encoding")? {
                s.extras.insert(props::ENCODING, enc);
            }
            if let Some(coll) = row.opt_string("collate_name")? {
                s.extras.insert(props::COLLATION, coll);
            }
            s.extras.insert(props::CURRENT, row.flag("is_current")?);
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
        (props::ENCODING, "encoding"),
        (props::COLLATION, "collate_name"),
        (CTYPE, "ctype"),
        (props::TABLESPACE, "tablespace"),
    ] {
        if let Some(v) = row.opt_string(column)? {
            def.extras.insert(prop, v);
        }
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
            s.system = is_system_schema(&name);
            s.comment = row.opt_string("comment")?;
            if row.flag("is_temp")? {
                s.extras.insert(props::TEMPORARY, true);
            }
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
    def.owner = row.opt_string("owner")?;
    def.comment = row.opt_string("comment")?;
    if row.flag("is_temp")? {
        def.extras.insert(props::TEMPORARY, true);
    }
    Ok(ObjectDef::Schema(def))
}

async fn list_extensions(cx: &Cx<'_>, parent: &ObjectRef) -> Result<Vec<ObjectSummary>> {
    expect_parent(parent, &[ObjectKind::Database], ObjectKind::Extension)?;
    cx.check_database(Some(&parent.name))?;
    let rows = cx.query(sql::EXTENSIONS, &[]).await?;
    rows.iter()
        .map(|row| {
            let name = row.string("name")?;
            let mut s = ObjectSummary::new(parent.child(ObjectKind::Extension, &name));
            s.system = name == "plpgsql";
            s.comment = row.opt_string("comment")?;
            if let Some(v) = row.opt_string("version")? {
                s.extras.insert(EXTENSION_VERSION, v);
            }
            Ok(s)
        })
        .collect()
}

async fn load_extension(cx: &Cx<'_>, obj: &ObjectRef) -> Result<ObjectDef> {
    let obj = cx.in_session_database(obj)?;
    let rows = cx
        .query(sql::EXTENSION, &[Value::from(obj.name.as_str())])
        .await?;
    let row = rows.first().ok_or_else(|| not_found(&obj))?;
    let mut def = GenericDef {
        obj,
        definition: None,
        comment: row.opt_string("comment")?,
        extras: Extras::new(),
    };
    for (prop, column) in [
        (EXTENSION_VERSION, "version"),
        (EXTENSION_SCHEMA, "schema_name"),
        (props::OWNER, "owner"),
    ] {
        if let Some(v) = row.opt_string(column)? {
            def.extras.insert(prop, v);
        }
    }
    Ok(ObjectDef::Generic(def))
}

// ---------------------------------------------------------------------------------------------
// Schema-level objects

/// The normalized schema parent of a schema-level listing.
fn schema_parent(cx: &Cx<'_>, parent: &ObjectRef, kind: ObjectKind) -> Result<ObjectRef> {
    expect_parent(parent, &[ObjectKind::Schema], kind)?;
    cx.in_session_database(parent)
}

async fn list_relations(
    cx: &Cx<'_>,
    kind: ObjectKind,
    parent: &ObjectRef,
) -> Result<Vec<ObjectSummary>> {
    let parent = schema_parent(cx, parent, kind)?;
    let sql = match kind {
        ObjectKind::Table => sql::TABLES,
        ObjectKind::View => sql::VIEWS,
        ObjectKind::MatView => sql::MATVIEWS,
        _ => sql::SEQUENCES,
    };
    let rows = cx.query(sql, &[Value::from(parent.name.as_str())]).await?;
    let system = is_system_schema(&parent.name);
    rows.iter()
        .map(|row| {
            let mut s = ObjectSummary::new(parent.child(kind, row.string("name")?));
            s.system = system || row.flag("extension_member")?;
            s.comment = row.opt_string("comment")?;
            if matches!(kind, ObjectKind::Table | ObjectKind::MatView) {
                s.row_estimate = row
                    .opt_i64("row_estimate")?
                    .and_then(|n| u64::try_from(n).ok());
            }
            if kind == ObjectKind::Table {
                table_type_extras(row, &mut s.extras)?;
            }
            if kind == ObjectKind::MatView {
                s.extras.insert(props::POPULATED, row.flag("populated")?);
            }
            Ok(s)
        })
        .collect()
}

/// `TABLE_TYPE`, `UNLOGGED` and `TEMPORARY` from a row with `relkind`, `is_partition` and
/// `persistence`.
fn table_type_extras(row: Row<'_>, extras: &mut Extras) -> Result<()> {
    let table_type = match row.code("relkind")? {
        "p" => Some("partitioned"),
        "f" => Some("foreign"),
        _ if row.flag("is_partition")? => Some("partition"),
        _ => None,
    };
    if let Some(t) = table_type {
        extras.insert(props::TABLE_TYPE, t);
    }
    match row.code("persistence")? {
        "u" => {
            extras.insert(props::UNLOGGED, true);
        }
        "t" => {
            extras.insert(props::TEMPORARY, true);
        }
        _ => {}
    }
    Ok(())
}

async fn load_table(cx: &Cx<'_>, obj: &ObjectRef) -> Result<ObjectDef> {
    let rel = find_relation(cx, obj)
        .await?
        .filter(|r| r.obj.kind == ObjectKind::Table)
        .ok_or_else(|| not_found(obj))?;
    let head = cx.query(sql::TABLE_HEAD, &[Value::Int(rel.oid)]).await?;
    let head = head.first().ok_or_else(|| not_found(obj))?;
    let mut def = TableDef::new(rel.obj.clone());
    def.comment = head.opt_string("comment")?;
    table_type_extras(head, &mut def.extras)?;
    for (prop, column) in [
        (props::OWNER, "owner"),
        (props::TABLESPACE, "tablespace"),
        (props::PARTITION_BY, "partition_by"),
    ] {
        if let Some(v) = head.opt_string(column)? {
            def.extras.insert(prop, v);
        }
    }
    def.columns = columns(cx, rel.oid)
        .await?
        .into_iter()
        .map(|c| c.def)
        .collect();
    let cons = constraints(cx, rel.oid, rel.obj.catalog.as_deref()).await?;
    for con in cons {
        match con.def {
            Constraint::PrimaryKey(pk) => def.primary_key = Some(pk),
            Constraint::ForeignKey(fk) => def.foreign_keys.push(fk),
            Constraint::Check(check) => def.checks.push(check),
            Constraint::Unique => {}
        }
    }
    def.indexes = indexes(cx, rel.oid)
        .await?
        .into_iter()
        .filter(|i| !i.primary)
        .map(|i| i.def)
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
    let row = rows
        .first()
        .filter(|row| relation_kind(row.code("relkind").unwrap_or("")) == Some(obj.kind))
        .ok_or_else(|| not_found(&obj))?;
    let mut extras = Extras::new();
    if let Some(owner) = row.opt_string("owner")? {
        extras.insert(props::OWNER, owner);
    }
    if let Some(mode) = row.opt_string("check_option")? {
        extras.insert(props::CHECK_OPTION, mode);
    }
    if obj.kind == ObjectKind::MatView {
        extras.insert(props::POPULATED, row.flag("populated")?);
        if let Some(ts) = row.opt_string("tablespace")? {
            extras.insert(props::TABLESPACE, ts);
        }
    }
    Ok(ObjectDef::View(ViewDef {
        definition: view_query(row.opt_str("definition")?.unwrap_or_default()),
        columns: Vec::new(),
        comment: row.opt_string("comment")?,
        extras,
        obj,
    }))
}

/// The query of a view as `pg_get_viewdef` prints it, without surrounding whitespace and the
/// trailing `;`.
fn view_query(text: &str) -> String {
    let t = text.trim();
    t.strip_suffix(';').unwrap_or(t).trim_end().to_owned()
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
    let mut def = SequenceDef::new(obj.clone());
    def.data_type = row.opt_string("data_type")?;
    def.start = i128::from(row.i64("start_value")?);
    def.increment = i128::from(row.i64("increment_by")?);
    def.min_value = row.opt_i64("min_value")?.map(i128::from);
    def.max_value = row.opt_i64("max_value")?.map(i128::from);
    def.cache = row.opt_i64("cache_size")?.map(i128::from);
    def.cycle = row.flag("is_cycle")?;
    def.comment = row.opt_string("comment")?;
    if let (Some(schema), Some(table), Some(column)) = (
        row.opt_str("owned_schema")?,
        row.opt_str("owned_table")?,
        row.opt_str("owned_column")?,
    ) {
        def.owned_by = Some(
            ObjectRef::table(obj.catalog.as_deref(), Some(schema), table)
                .child(ObjectKind::Column, column),
        );
    }
    if row.code("dep_type")? == "i" {
        def.extras.insert(IDENTITY_SEQUENCE, true);
    }
    if let Some(owner) = row.opt_string("owner")? {
        def.extras.insert(props::OWNER, owner);
    }
    Ok(ObjectDef::Sequence(def))
}

async fn list_types(cx: &Cx<'_>, parent: &ObjectRef) -> Result<Vec<ObjectSummary>> {
    let parent = schema_parent(cx, parent, ObjectKind::Type)?;
    let rows = cx
        .query(sql::TYPES, &[Value::from(parent.name.as_str())])
        .await?;
    let system = is_system_schema(&parent.name);
    rows.iter()
        .map(|row| {
            let mut s = ObjectSummary::new(parent.child(ObjectKind::Type, row.string("name")?));
            s.system = system || row.flag("extension_member")?;
            s.comment = row.opt_string("comment")?;
            if let Some(kind) = type_kind(row.code("typtype")?) {
                s.extras.insert(TYPE_KIND, kind);
            }
            Ok(s)
        })
        .collect()
}

fn type_kind(typtype: &str) -> Option<&'static str> {
    match typtype {
        "e" => Some("enum"),
        "c" => Some("composite"),
        "d" => Some("domain"),
        "r" => Some("range"),
        "m" => Some("multirange"),
        _ => None,
    }
}

async fn load_type(cx: &Cx<'_>, obj: &ObjectRef) -> Result<ObjectDef> {
    let obj = cx.in_session_database(obj)?;
    let schema = schema_of(&obj)?;
    let rows = cx
        .query(
            sql::TYPE,
            &[Value::from(schema), Value::from(obj.name.as_str())],
        )
        .await?;
    let row = rows.first().ok_or_else(|| not_found(&obj))?;
    let mut extras = Extras::new();
    if let Some(kind) = type_kind(row.code("typtype")?) {
        extras.insert(TYPE_KIND, kind);
    }
    for (prop, column) in [
        (props::OWNER, "owner"),
        (BASE_TYPE, "base_type"),
        (ENUM_LABELS, "enum_labels"),
    ] {
        if let Some(v) = row.opt_string(column)? {
            extras.insert(prop, v);
        }
    }
    Ok(ObjectDef::Generic(GenericDef {
        obj,
        definition: None,
        comment: row.opt_string("comment")?,
        extras,
    }))
}

// ---------------------------------------------------------------------------------------------
// Routines

async fn list_routines(
    cx: &Cx<'_>,
    kind: ObjectKind,
    parent: &ObjectRef,
) -> Result<Vec<ObjectSummary>> {
    let parent = schema_parent(cx, parent, kind)?;
    let sql = if kind == ObjectKind::Procedure {
        if !sql::at_least(cx.version(), 11) {
            // Procedures arrived in PostgreSQL 11.
            return Ok(Vec::new());
        }
        sql::procedures(false)
    } else {
        sql::functions(cx.version(), false)
    };
    let rows = cx.query(sql, &[Value::from(parent.name.as_str())]).await?;
    let system = is_system_schema(&parent.name);
    rows.iter()
        .map(|row| {
            let r = parent
                .child(kind, row.string("name")?)
                .with_signature(row.opt_string("signature")?.unwrap_or_default());
            let mut s = ObjectSummary::new(r);
            s.system = system || row.flag("extension_member")?;
            s.comment = row.opt_string("comment")?;
            Ok(s)
        })
        .collect()
}

async fn load_routine(cx: &Cx<'_>, obj: &ObjectRef) -> Result<ObjectDef> {
    let obj = cx.in_session_database(obj)?;
    let schema = schema_of(&obj)?;
    let by_name = if obj.kind == ObjectKind::Procedure {
        if !sql::at_least(cx.version(), 11) {
            return Err(not_found(&obj));
        }
        sql::procedures(true)
    } else {
        sql::functions(cx.version(), true)
    };
    let candidates = cx
        .query(
            by_name,
            &[Value::from(schema), Value::from(obj.name.as_str())],
        )
        .await?;
    let mut matching = Vec::new();
    for row in candidates.iter() {
        let signature = row.opt_string("signature")?.unwrap_or_default();
        if obj.signature.as_ref().is_none_or(|s| *s == signature) {
            matching.push((row.i64("oid")?, signature));
        }
    }
    let (oid, signature) = match matching.len() {
        0 => return Err(not_found(&obj)),
        1 => matching.remove(0),
        n => {
            return Err(Error::invalid_input(format!(
                "{} {} has {n} overloads; give its signature",
                obj.kind.display_name(),
                obj.path().join(".")
            )));
        }
    };

    let detail = cx
        .query(sql::routine(cx.version()), &[Value::Int(oid)])
        .await?;
    let row = detail.first().ok_or_else(|| not_found(&obj))?;
    let mut def = RoutineDef::new(obj.clone().with_signature(signature));
    if obj.kind == ObjectKind::Function {
        def.returns = row.opt_string("returns")?;
    }
    def.language = row.opt_string("language")?;
    def.body = match row.opt_string("sql_body")? {
        Some(body) => Some(body),
        None => row.opt_string("src")?,
    };
    def.definition = row.opt_string("definition")?;
    def.comment = row.opt_string("comment")?;
    let volatility = match row.code("volatility")? {
        "i" => Some("immutable"),
        "s" => Some("stable"),
        "v" => Some("volatile"),
        _ => None,
    };
    if let Some(v) = volatility {
        def.extras.insert(props::VOLATILITY, v);
    }
    def.extras
        .insert(props::STRICT_INPUT, row.flag("is_strict")?);
    def.extras.insert(
        props::SECURITY,
        if row.flag("security_definer")? {
            "definer"
        } else {
            "invoker"
        },
    );
    let parallel = match row.code("parallel")? {
        "s" => Some("safe"),
        "r" => Some("restricted"),
        "u" => Some("unsafe"),
        _ => None,
    };
    if let Some(p) = parallel {
        def.extras.insert(PARALLEL, p);
    }
    if let Some(owner) = row.opt_string("owner")? {
        def.extras.insert(props::OWNER, owner);
    }

    let args = cx.query(sql::ROUTINE_ARGS, &[Value::Int(oid)]).await?;
    def.args = args
        .iter()
        .map(|row| {
            Ok(RoutineArg {
                name: row.opt_string("name")?.filter(|n| !n.is_empty()),
                mode: arg_mode(row.code("mode")?),
                native_type: row.string("native_type")?,
                default: row.opt_string("default_expr")?,
            })
        })
        .collect::<Result<_>>()?;
    Ok(ObjectDef::Routine(def))
}

fn arg_mode(mode: &str) -> ArgMode {
    match mode {
        "o" => ArgMode::Out,
        "b" => ArgMode::InOut,
        "v" => ArgMode::Variadic,
        "t" => ArgMode::Table,
        _ => ArgMode::In,
    }
}

// ---------------------------------------------------------------------------------------------
// Table children

async fn list_table_children(
    cx: &Cx<'_>,
    kind: ObjectKind,
    parent: &ObjectRef,
) -> Result<Vec<ObjectSummary>> {
    expect_parent(
        parent,
        &[ObjectKind::Table, ObjectKind::View, ObjectKind::MatView],
        kind,
    )?;
    let parent = cx.in_session_database(parent)?;
    let rel = find_relation(cx, &parent)
        .await?
        .ok_or_else(|| not_found(&parent))?;
    let system = is_system_schema(schema_of(&parent)?) || rel.extension_member;
    // Refs keep the kind the caller used for the parent: it is how the tree addresses them.
    let summary = |name: &str, comment: Option<String>| {
        let mut s = ObjectSummary::new(parent.child(kind, name));
        s.system = system;
        s.comment = comment;
        s
    };
    let out = match kind {
        ObjectKind::Column => columns(cx, rel.oid)
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
        ObjectKind::Index => indexes(cx, rel.oid)
            .await?
            .into_iter()
            .map(|i| {
                let mut s = summary(&i.def.name, i.comment);
                s.extras.insert(props::PRIMARY_KEY, i.primary);
                s.extras.insert(props::UNIQUE, i.def.unique);
                if let Some(m) = i.def.extras.get(&props::INDEX_METHOD) {
                    s.extras.insert(props::INDEX_METHOD, m.clone());
                }
                s
            })
            .collect(),
        ObjectKind::ForeignKey | ObjectKind::Check => {
            constraints(cx, rel.oid, parent.catalog.as_deref())
                .await?
                .into_iter()
                .filter(|con| con.kind() == Some(kind))
                .map(|con| summary(&con.name, con.comment))
                .collect()
        }
        _ => triggers(cx, rel.oid, parent.catalog.as_deref())
            .await?
            .into_iter()
            .map(|t| summary(&t.name, t.comment))
            .collect(),
    };
    Ok(out)
}

async fn load_table_child(cx: &Cx<'_>, obj: &ObjectRef) -> Result<ObjectDef> {
    let obj = cx.in_session_database(obj)?;
    let owner = obj
        .owner()
        .filter(|o| !o.name.is_empty())
        .ok_or_else(|| Error::invalid_input(format!("{obj} has no owning table")))?;
    let rel = find_relation(cx, &owner)
        .await?
        .ok_or_else(|| not_found(&obj))?;
    let table = rel.obj.clone();
    match obj.kind {
        ObjectKind::Column => {
            let column = columns(cx, rel.oid)
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
            let index = indexes(cx, rel.oid)
                .await?
                .into_iter()
                .find(|i| i.def.name == obj.name)
                .ok_or_else(|| not_found(&obj))?;
            Ok(ObjectDef::Index {
                table,
                index: index.def,
            })
        }
        ObjectKind::ForeignKey | ObjectKind::Check => {
            let con = constraints(cx, rel.oid, table.catalog.as_deref())
                .await?
                .into_iter()
                .find(|c| c.name == obj.name && c.kind() == Some(obj.kind))
                .ok_or_else(|| not_found(&obj))?;
            match con.def {
                Constraint::ForeignKey(foreign_key) => {
                    Ok(ObjectDef::ForeignKey { table, foreign_key })
                }
                Constraint::Check(check) => Ok(ObjectDef::Check { table, check }),
                Constraint::PrimaryKey(_) | Constraint::Unique => Err(not_found(&obj)),
            }
        }
        _ => {
            let mut trigger = triggers(cx, rel.oid, table.catalog.as_deref())
                .await?
                .into_iter()
                .find(|t| t.name == obj.name)
                .ok_or_else(|| not_found(&obj))?;
            if trigger.def.events.contains(&TriggerEvent::Update) {
                let cols = cx
                    .query(sql::TRIGGER_COLUMNS, &[Value::Int(trigger.oid)])
                    .await?;
                trigger.def.update_columns = cols
                    .iter()
                    .map(|r| r.string("name"))
                    .collect::<Result<_>>()?;
            }
            trigger.def.obj = table.child(ObjectKind::Trigger, &trigger.name);
            Ok(ObjectDef::Trigger(trigger.def))
        }
    }
}

/// One column with listing facts.
struct Column {
    def: ColumnDef,
    in_primary_key: bool,
}

async fn columns(cx: &Cx<'_>, oid: i64) -> Result<Vec<Column>> {
    let rows = cx
        .query(sql::columns(cx.version()), &[Value::Int(oid)])
        .await?;
    rows.iter().map(column).collect()
}

fn column(row: Row<'_>) -> Result<Column> {
    let native_type = row.string("native_type")?;
    let mut def = ColumnDef::new(row.string("name")?, types::data_type(&native_type));
    def.native_type = native_type;
    def.nullable = row.flag("nullable")?;
    let default = row.opt_string("default_expr")?;
    // `s`: stored, `v`: virtual (18+); the generation expression is kept as the default.
    let generated = row.code("generated")?;
    if generated == "s" || generated == "v" {
        def.generated = Some(GeneratedColumn {
            expression: default.unwrap_or_default(),
            stored: generated == "s",
        });
    } else {
        def.default = default;
    }
    // `a`: GENERATED ALWAYS, `d`: BY DEFAULT.
    let identity = row.code("identity")?;
    if identity == "a" || identity == "d" {
        def.identity = Some(IdentityDef {
            always: identity == "a",
            start: row.opt_i64("identity_start")?.map(i128::from),
            increment: row.opt_i64("identity_increment")?.map(i128::from),
        });
        def.auto_increment = true;
    } else {
        // `serial`: a `nextval` default on a sequence the column owns.
        def.auto_increment = row.flag("owns_sequence")?
            && def
                .default
                .as_deref()
                .is_some_and(|d| d.starts_with("nextval("));
    }
    def.collation = row.opt_string("collation_name")?;
    def.comment = row.opt_string("comment")?;
    Ok(Column {
        def,
        in_primary_key: row.flag("in_primary_key")?,
    })
}

/// A constraint of a table.
enum Constraint {
    PrimaryKey(PrimaryKeyDef),
    /// A `UNIQUE` constraint: read through its index.
    Unique,
    ForeignKey(ForeignKeyDef),
    Check(CheckDef),
}

struct NamedConstraint {
    name: String,
    comment: Option<String>,
    def: Constraint,
}

impl NamedConstraint {
    fn kind(&self) -> Option<ObjectKind> {
        match self.def {
            Constraint::ForeignKey(_) => Some(ObjectKind::ForeignKey),
            Constraint::Check(_) => Some(ObjectKind::Check),
            Constraint::PrimaryKey(_) | Constraint::Unique => None,
        }
    }
}

fn fk_action(code: &str) -> FkAction {
    match code {
        "r" => FkAction::Restrict,
        "c" => FkAction::Cascade,
        "n" => FkAction::SetNull,
        "d" => FkAction::SetDefault,
        _ => FkAction::NoAction,
    }
}

/// The constraints of relation `oid` in name order. `catalog` is the database of referenced
/// tables.
async fn constraints(cx: &Cx<'_>, oid: i64, catalog: Option<&str>) -> Result<Vec<NamedConstraint>> {
    let rows = cx
        .query(sql::constraints(cx.version()), &[Value::Int(oid)])
        .await?;
    let col_rows = cx
        .query(sql::CONSTRAINT_COLUMNS, &[Value::Int(oid)])
        .await?;
    // (constraint oid, referenced side) -> columns in key order. Entries of constraints the
    // first query leaves out (partition clones of foreign keys) are never looked up.
    let mut cols: BTreeMap<(i64, bool), Vec<String>> = BTreeMap::new();
    for row in col_rows.iter() {
        cols.entry((row.i64("con")?, row.flag("is_referenced")?))
            .or_default()
            .push(row.string("name")?);
    }
    let mut out = Vec::new();
    for row in rows.iter() {
        let con_oid = row.i64("oid")?;
        let name = row.string("name")?;
        let key = cols.remove(&(con_oid, false)).unwrap_or_default();
        let mut extras = Extras::new();
        // NOT ENFORCED constraints are never validated; NOT VALID is about enforced ones.
        if row.flag("is_enforced")? {
            if !row.flag("is_validated")? {
                extras.insert(NOT_VALID, true);
            }
        } else {
            extras.insert(NOT_ENFORCED, true);
        }
        let def = match row.code("contype")? {
            "p" => Constraint::PrimaryKey(PrimaryKeyDef {
                name: Some(name.clone()),
                columns: key,
            }),
            "u" => Constraint::Unique,
            "f" => {
                extras.insert(props::DEFERRABLE, row.flag("is_deferrable")?);
                extras.insert(props::INITIALLY_DEFERRED, row.flag("is_deferred")?);
                match row.code("match_type")? {
                    "f" => {
                        extras.insert(MATCH_TYPE, "full");
                    }
                    "p" => {
                        extras.insert(MATCH_TYPE, "partial");
                    }
                    _ => {}
                }
                Constraint::ForeignKey(ForeignKeyDef {
                    name: Some(name.clone()),
                    columns: key,
                    ref_table: ObjectRef::table(
                        catalog,
                        row.opt_str("ref_schema")?,
                        row.string("ref_table")?,
                    ),
                    ref_columns: cols.remove(&(con_oid, true)).unwrap_or_default(),
                    on_delete: fk_action(row.code("on_delete")?),
                    on_update: fk_action(row.code("on_update")?),
                    extras,
                })
            }
            _ => {
                if row.flag("no_inherit")? {
                    extras.insert(NO_INHERIT, true);
                }
                Constraint::Check(CheckDef {
                    name: Some(name.clone()),
                    expression: row.opt_string("check_expr")?.unwrap_or_default(),
                    extras,
                })
            }
        };
        out.push(NamedConstraint {
            name,
            comment: row.opt_string("comment")?,
            def,
        });
    }
    Ok(out)
}

/// One column or expression of an index, as the catalog lists it.
struct IndexPart {
    /// 1-based position; positions past the key count are `INCLUDE` columns.
    ord: i64,
    /// The column name; `None` for an expression.
    column: Option<String>,
    /// The column or expression text (`pg_get_indexdef`).
    text: String,
    descending: bool,
    nulls_first: bool,
}

/// One index with listing facts.
struct Index {
    def: IndexDef,
    primary: bool,
    comment: Option<String>,
}

async fn indexes(cx: &Cx<'_>, oid: i64) -> Result<Vec<Index>> {
    let rows = cx
        .query(sql::indexes(cx.version()), &[Value::Int(oid)])
        .await?;
    let col_rows = cx.query(sql::INDEX_COLUMNS, &[Value::Int(oid)]).await?;
    // index oid -> its columns in index order
    let mut cols: BTreeMap<i64, Vec<IndexPart>> = BTreeMap::new();
    for row in col_rows.iter() {
        cols.entry(row.i64("idx")?).or_default().push(IndexPart {
            ord: row.i64("ord")?,
            column: row.opt_string("name")?,
            text: row.opt_string("expr")?.unwrap_or_default(),
            descending: row.flag("descending")?,
            nulls_first: row.flag("nulls_first")?,
        });
    }
    let mut out = Vec::new();
    for row in rows.iter() {
        let key_count = row.i64("key_count")?;
        let mut columns = Vec::new();
        let mut include = Vec::new();
        let mut nulls_first = Vec::new();
        for part in cols.remove(&row.i64("oid")?).unwrap_or_default() {
            if part.ord > key_count {
                include.push(part.column.unwrap_or(part.text));
                continue;
            }
            nulls_first.push((part.nulls_first, part.descending));
            columns.push(IndexColumn {
                is_expression: part.column.is_none(),
                name: part.column.unwrap_or(part.text),
                descending: part.descending,
                length: None,
            });
        }
        let mut extras = Extras::new();
        if let Some(method) = row.opt_string("method")? {
            extras.insert(props::INDEX_METHOD, method);
        }
        if !include.is_empty() {
            extras.insert(
                props::INCLUDE_COLUMNS,
                Value::Json(serde_json::Value::from(include)),
            );
        }
        if nulls_first.iter().any(|(first, desc)| first != desc) {
            let flags: Vec<bool> = nulls_first.iter().map(|(first, _)| *first).collect();
            extras.insert(NULLS_FIRST, Value::Json(serde_json::Value::from(flags)));
        }
        if let Some(exclusion) = row.opt_string("exclusion")? {
            extras.insert(EXCLUSION, exclusion);
        }
        if row.flag("nulls_not_distinct")? {
            extras.insert(NULLS_NOT_DISTINCT, true);
        }
        if !row.flag("is_valid")? {
            extras.insert(NOT_VALID, true);
        }
        out.push(Index {
            def: IndexDef {
                name: row.string("name")?,
                columns,
                unique: row.flag("is_unique")?,
                is_constraint: row.flag("is_constraint")?,
                predicate: row.opt_string("predicate")?,
                extras,
            },
            primary: row.flag("is_primary")?,
            comment: row.opt_string("comment")?,
        });
    }
    Ok(out)
}

/// One trigger, without its `UPDATE OF` columns (read on `load` only).
struct Trigger {
    oid: i64,
    name: String,
    comment: Option<String>,
    def: TriggerDef,
}

// `pg_trigger.tgtype` bits.
const TG_ROW: i64 = 1;
const TG_BEFORE: i64 = 1 << 1;
const TG_INSERT: i64 = 1 << 2;
const TG_DELETE: i64 = 1 << 3;
const TG_UPDATE: i64 = 1 << 4;
const TG_TRUNCATE: i64 = 1 << 5;
const TG_INSTEAD: i64 = 1 << 6;

/// Timing, events and level from `tgtype`.
fn trigger_shape(tgtype: i64) -> (TriggerTiming, Vec<TriggerEvent>, TriggerLevel) {
    let timing = if tgtype & TG_INSTEAD != 0 {
        TriggerTiming::InsteadOf
    } else if tgtype & TG_BEFORE != 0 {
        TriggerTiming::Before
    } else {
        TriggerTiming::After
    };
    let events = [
        (TG_INSERT, TriggerEvent::Insert),
        (TG_UPDATE, TriggerEvent::Update),
        (TG_DELETE, TriggerEvent::Delete),
        (TG_TRUNCATE, TriggerEvent::Truncate),
    ]
    .into_iter()
    .filter(|(bit, _)| tgtype & bit != 0)
    .map(|(_, e)| e)
    .collect();
    let level = if tgtype & TG_ROW != 0 {
        TriggerLevel::Row
    } else {
        TriggerLevel::Statement
    };
    (timing, events, level)
}

/// The user triggers of relation `oid` in name order. Trigger and function refs get catalog
/// `catalog`; trigger refs are re-parented by the caller.
async fn triggers(cx: &Cx<'_>, oid: i64, catalog: Option<&str>) -> Result<Vec<Trigger>> {
    let rows = cx.query(sql::TRIGGERS, &[Value::Int(oid)]).await?;
    rows.iter()
        .map(|row| {
            let name = row.string("name")?;
            let (timing, events, level) = trigger_shape(row.i64("tgtype")?);
            let function = ObjectRef::schema(catalog, row.string("fn_schema")?)
                .child(ObjectKind::Function, row.string("fn_name")?)
                .with_signature(row.opt_string("fn_signature")?.unwrap_or_default());
            let comment = row.opt_string("comment")?;
            let definition = row.opt_string("definition")?;
            Ok(Trigger {
                oid: row.i64("oid")?,
                def: TriggerDef {
                    obj: ObjectRef::new(ObjectKind::Trigger, &name),
                    timing,
                    events,
                    update_columns: Vec::new(),
                    level,
                    condition: definition.as_deref().and_then(trigger_condition),
                    function: Some(function),
                    body: None,
                    definition,
                    enabled: row.code("enabled")? != "D",
                    comment: comment.clone(),
                    extras: Extras::new(),
                },
                name,
                comment,
            })
        })
        .collect()
}

/// The `WHEN (...)` condition of a trigger, without `WHEN` and the parentheses, cut out of its
/// pretty-printed definition (`pg_get_triggerdef(oid, true)`). The condition follows
/// `FOR EACH ROW` / `FOR EACH STATEMENT`; quoted names and string literals are skipped while
/// looking for that clause and for the closing parenthesis, so names, literals or nested
/// `CASE WHEN (...)` expressions do not cut it short.
fn trigger_condition(definition: &str) -> Option<String> {
    const WHEN: [&[u8]; 2] = [b" FOR EACH ROW WHEN (", b" FOR EACH STATEMENT WHEN ("];
    let b = definition.as_bytes();
    let mut i = 0;
    let open = loop {
        if i >= b.len() {
            return None;
        }
        if b[i] == b'"' || b[i] == b'\'' {
            i = skip_quoted(b, i);
            continue;
        }
        let rest = &b[i..];
        if let Some(m) = WHEN.iter().find(|m| rest.starts_with(m)) {
            break i + m.len();
        }
        if rest.starts_with(b" FOR EACH ") {
            // The level clause without a WHEN.
            return None;
        }
        i += 1;
    };
    let mut depth = 1_usize;
    let mut j = open;
    while j < b.len() {
        match b[j] {
            b'"' | b'\'' => {
                j = skip_quoted(b, j);
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    // `open` and `j` sit next to ASCII bytes: both are char boundaries.
                    return Some(definition[open..j].trim().to_owned());
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// The index just past the quoted text starting at `start` (a `"` identifier or `'` literal; a
/// doubled quote inside is an escaped one). Unterminated text runs to the end.
fn skip_quoted(b: &[u8], start: usize) -> usize {
    let quote = b[start];
    let mut i = start + 1;
    while i < b.len() {
        if b[i] == quote {
            if b.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_conditions_are_cut_out_of_the_definition() {
        let head = "CREATE TRIGGER t BEFORE UPDATE OF a ON s.t";
        let tail = "EXECUTE FUNCTION s.f()";
        assert_eq!(
            trigger_condition(&format!("{head} FOR EACH ROW {tail}")),
            None
        );
        assert_eq!(
            trigger_condition(&format!(
                "{head} FOR EACH ROW WHEN (old.a IS DISTINCT FROM new.a) {tail}"
            ))
            .as_deref(),
            Some("old.a IS DISTINCT FROM new.a")
        );
        // A nested `CASE WHEN (...)` and a literal with parentheses in the arguments.
        assert_eq!(
            trigger_condition(&format!(
                "{head} FOR EACH ROW WHEN (CASE WHEN (new.a > 0) THEN true ELSE false END) \
                 EXECUTE FUNCTION s.f(')')"
            ))
            .as_deref(),
            Some("CASE WHEN (new.a > 0) THEN true ELSE false END")
        );
        // Multi-line pretty output.
        assert_eq!(
            trigger_condition(&format!(
                "{head} FOR EACH STATEMENT WHEN (\n    CASE\n    WHEN x THEN true\n    END) {tail}"
            ))
            .as_deref(),
            Some("CASE\n    WHEN x THEN true\n    END")
        );
        // Quoted names holding the marker, literals holding parentheses and quotes.
        assert_eq!(
            trigger_condition(
                "CREATE TRIGGER \"x FOR EACH ROW WHEN (\" AFTER UPDATE ON \"s\"\")\".\"é\" \
                 FOR EACH ROW WHEN (new.t = ')(''' AND new.u <> 'a''b') EXECUTE FUNCTION f()"
            )
            .as_deref(),
            Some("new.t = ')(''' AND new.u <> 'a''b'")
        );
        // Unbalanced text gives nothing rather than a wrong condition.
        assert_eq!(
            trigger_condition(&format!("{head} FOR EACH ROW WHEN (new.a > (1) {tail}")),
            None
        );
        assert_eq!(trigger_condition(""), None);
    }

    #[test]
    fn trigger_bits() {
        // BEFORE INSERT FOR EACH ROW.
        let (timing, events, level) = trigger_shape(TG_ROW | TG_BEFORE | TG_INSERT);
        assert_eq!(timing, TriggerTiming::Before);
        assert_eq!(events, [TriggerEvent::Insert]);
        assert_eq!(level, TriggerLevel::Row);
        // AFTER DELETE OR UPDATE OR TRUNCATE FOR EACH STATEMENT: events come out sorted.
        let (timing, events, level) = trigger_shape(TG_DELETE | TG_UPDATE | TG_TRUNCATE);
        assert_eq!(timing, TriggerTiming::After);
        assert_eq!(
            events,
            [
                TriggerEvent::Update,
                TriggerEvent::Delete,
                TriggerEvent::Truncate
            ]
        );
        assert_eq!(level, TriggerLevel::Statement);
        let (timing, ..) = trigger_shape(TG_ROW | TG_INSTEAD | TG_UPDATE);
        assert_eq!(timing, TriggerTiming::InsteadOf);
    }

    #[test]
    fn small_mappings() {
        assert_eq!(fk_action("c"), FkAction::Cascade);
        assert_eq!(fk_action("a"), FkAction::NoAction);
        assert_eq!(fk_action("n"), FkAction::SetNull);
        assert_eq!(fk_action("d"), FkAction::SetDefault);
        assert_eq!(fk_action("r"), FkAction::Restrict);
        assert_eq!(arg_mode("i"), ArgMode::In);
        assert_eq!(arg_mode("b"), ArgMode::InOut);
        assert_eq!(arg_mode("t"), ArgMode::Table);
        assert_eq!(relation_kind("p"), Some(ObjectKind::Table));
        assert_eq!(relation_kind("m"), Some(ObjectKind::MatView));
        assert_eq!(relation_kind("i"), None);
        assert!(is_system_schema("pg_catalog"));
        assert!(is_system_schema("pg_toast"));
        assert!(is_system_schema("information_schema"));
        assert!(!is_system_schema("public"));
        assert!(!is_system_schema("pg_temp_3"));
        assert_eq!(
            view_query(" SELECT 1 AS one;\n"),
            "SELECT 1 AS one".to_owned()
        );
        assert_eq!(view_query("SELECT 2"), "SELECT 2".to_owned());
    }

    #[test]
    fn parents_are_checked() {
        let schema = ObjectRef::schema(Some("db"), "s");
        assert!(expect_parent(&schema, &[ObjectKind::Schema], ObjectKind::Table).is_ok());
        let err = expect_parent(
            &ObjectRef::root(),
            &[ObjectKind::Database],
            ObjectKind::Schema,
        )
        .unwrap_err();
        assert!(err.to_string().contains("schemas"), "{err}");
        assert!(
            expect_parent(
                &schema.child(ObjectKind::Table, "t"),
                &[ObjectKind::Schema],
                ObjectKind::View
            )
            .is_err()
        );
    }
}
