//! MongoDB catalog fetchers (mongo stream), as described in the [`super`] module docs. They
//! reach the client with `Connection::as_any` → [`MongoConnection`](crate::MongoConnection)
//! and run commands with the connection's operation comment, honouring the token between
//! round trips (`Error::Cancelled`, nothing partial).
//!
//! Every command names its objects as BSON values (`{listIndexes: <name>}`, `{usersInfo:
//! {user, db}}`), never as code. A cancel while a command runs kills it by its comment
//! (`MongoConnection::kill_operation`) and ends the call with `Cancelled` at once.
//!
//! This module also holds the small helpers the other cells of this crate share (the
//! connection downcast, the cancellable command wrapper, the error mapping).

use std::collections::HashMap;
use std::future::Future;

use arrow_schema::DataType as ArrowType;
use async_trait::async_trait;
use bson::{Bson, Document, doc};
use futures::{StreamExt, TryStreamExt};
use oxbase_dialect::Fetcher;
use oxbase_dialect::document::{DOCUMENT_COLUMN, META_BSON_TYPE};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    ColumnDef, DataType, Error, Extras, GenericDef, IndexColumn, IndexDef, ObjectDef, ObjectKind,
    ObjectRef, ObjectSummary, PrimaryKeyDef, Result, SchemaDef, TableDef, Value, ViewDef, props,
};

use crate::MongoConnection;
use crate::arrow::DocumentLayout;
use crate::driver::error::{Redactor, map_error};
use crate::ejson::to_canonical_text;
use crate::extras;

/// Databases MongoDB itself uses.
const SYSTEM_DATABASES: [&str; 3] = ["admin", "config", "local"];
/// Server error code: the collection or database does not exist.
const NAMESPACE_NOT_FOUND: i32 = 26;
/// Document counts the collection listing reads at the same time.
const COUNT_CONCURRENCY: usize = 4;
/// Metadata key of a binary column's subtype (`driver::convert`).
const META_BSON_SUBTYPE: &str = "oxbase.bson_subtype";

/// The fetcher of one MongoDB object kind.
#[derive(Debug, Clone, Copy)]
pub struct MongoFetcher {
    kind: ObjectKind,
}

impl MongoFetcher {
    pub fn new(kind: ObjectKind) -> Self {
        Self { kind }
    }

    /// The kind this fetcher lists.
    pub fn kind(&self) -> ObjectKind {
        self.kind
    }
}

#[async_trait]
impl Fetcher for MongoFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        check(&cancel)?;
        let conn = mongo(c)?;
        let mut out = match self.kind {
            ObjectKind::Database => list_databases(conn, &cancel).await?,
            ObjectKind::Table => {
                let db = database_of(conn, parent)?;
                list_collections(conn, &db, &cancel).await?
            }
            ObjectKind::View => {
                let db = database_of(conn, parent)?;
                list_views(conn, &db, &cancel).await?
            }
            ObjectKind::Index => list_indexes(conn, parent, &cancel).await?,
            ObjectKind::User => {
                let db = database_of(conn, parent)?;
                list_users(conn, &db, &cancel).await?
            }
            ObjectKind::Role => {
                let db = database_of(conn, parent)?;
                list_roles(conn, &db, &cancel).await?
            }
            other => {
                return Err(Error::unsupported(format!(
                    "MongoDB has no {}",
                    other.plural_display_name().to_lowercase()
                )));
            }
        };
        out.sort_by(|a, b| a.obj.name.cmp(&b.obj.name));
        check(&cancel)?;
        Ok(out)
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        check(&cancel)?;
        let conn = mongo(c)?;
        let def = match self.kind {
            ObjectKind::Database => load_database(conn, obj, &cancel).await?,
            ObjectKind::Table => load_collection(conn, obj, &cancel).await?,
            ObjectKind::View => load_view(conn, obj, &cancel).await?,
            ObjectKind::Index => load_index(conn, obj, &cancel).await?,
            ObjectKind::User => load_user(conn, obj, &cancel).await?,
            ObjectKind::Role => load_role(conn, obj, &cancel).await?,
            other => {
                return Err(Error::unsupported(format!(
                    "MongoDB has no {}",
                    other.plural_display_name().to_lowercase()
                )));
            }
        };
        check(&cancel)?;
        Ok(def)
    }
}

// ---- shared helpers ----------------------------------------------------------------------------

/// The MongoDB connection behind `c` (cells of this crate are only called with one).
pub(crate) fn mongo(c: &dyn Connection) -> Result<&MongoConnection> {
    c.as_any()
        .and_then(|any| any.downcast_ref::<MongoConnection>())
        .ok_or_else(|| Error::internal("a MongoDB cell was called with another engine's session"))
}

/// `Cancelled` once `cancel` fired.
pub(crate) fn check(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// A `mongodb` error as an oxbase error. Server texts never carry the password.
pub(crate) fn map(e: &mongodb::error::Error) -> Error {
    if let mongodb::error::ErrorKind::Command(c) = e.kind.as_ref()
        && c.code == NAMESPACE_NOT_FOUND
    {
        return Error::not_found(c.message.clone());
    }
    map_error(e, &Redactor::default())
}

/// Runs `work` (one or more round trips tagged with `comment`) unless `cancel` fires first; a
/// cancel kills the server operation by its comment and returns `Cancelled` at once.
pub(crate) async fn guarded<T>(
    conn: &MongoConnection,
    comment: &str,
    cancel: &CancellationToken,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    check(cancel)?;
    tokio::select! {
        biased;
        () = cancel.cancelled() => {
            if let Err(e) = conn.kill_operation(comment).await {
                tracing::debug!(error = %e, "MongoDB cell: killOp after a cancel failed");
            }
            Err(Error::Cancelled)
        }
        result = work => result,
    }
}

/// The database `obj` lives in (its catalog, or a database ref's own name), else the
/// session's current database.
pub(crate) fn database_of(conn: &MongoConnection, obj: &ObjectRef) -> Result<String> {
    obj.database_name()
        .filter(|d| !d.is_empty())
        .map(str::to_owned)
        .or_else(|| conn.current_database())
        .ok_or_else(|| Error::invalid_input(format!("{} names no database", obj.display_name())))
}

/// Runs one command on `db` with a fresh operation comment.
async fn command(
    conn: &MongoConnection,
    db: &str,
    mut cmd: Document,
    cancel: &CancellationToken,
) -> Result<Document> {
    let comment = conn.next_comment();
    cmd.insert("comment", comment.as_str());
    let database = conn.client().database(db);
    guarded(conn, &comment, cancel, async move {
        database.run_command(cmd).await.map_err(|e| map(&e))
    })
    .await
}

/// Runs one cursor command on `db` and reads every document of the cursor.
async fn cursor_command(
    conn: &MongoConnection,
    db: &str,
    mut cmd: Document,
    cancel: &CancellationToken,
) -> Result<Vec<Document>> {
    let comment = conn.next_comment();
    cmd.insert("comment", comment.as_str());
    let database = conn.client().database(db);
    guarded(conn, &comment, cancel, async move {
        let cursor = database
            .run_cursor_command(cmd)
            .await
            .map_err(|e| map(&e))?;
        cursor.try_collect().await.map_err(|e| map(&e))
    })
    .await
}

/// An integer of a reply (servers send counts and sizes as int32, int64 or double).
pub(crate) fn int(value: Option<&Bson>) -> Option<i64> {
    match value? {
        Bson::Int32(n) => Some(i64::from(*n)),
        Bson::Int64(n) => Some(*n),
        #[allow(
            clippy::cast_possible_truncation,
            reason = "sizes and counts are whole"
        )]
        Bson::Double(n) if n.is_finite() => Some(n.round() as i64),
        _ => None,
    }
}

fn json(value: &Bson) -> Value {
    Value::Json(value.clone().into_relaxed_extjson())
}

fn is_system_collection(name: &str) -> bool {
    name.starts_with("system.")
}

// ---- listings -----------------------------------------------------------------------------------

async fn list_databases(
    conn: &MongoConnection,
    cancel: &CancellationToken,
) -> Result<Vec<ObjectSummary>> {
    let reply = command(
        conn,
        "admin",
        doc! {"listDatabases": 1_i32, "nameOnly": false, "authorizedDatabases": true},
        cancel,
    )
    .await?;
    let databases = reply
        .get_array("databases")
        .map_err(|_| Error::internal("listDatabases returned no database list"))?;
    let mut out = Vec::with_capacity(databases.len());
    for entry in databases {
        let Bson::Document(d) = entry else {
            continue;
        };
        let Ok(name) = d.get_str("name") else {
            continue;
        };
        let mut summary = ObjectSummary::new(ObjectRef::database(name));
        summary.system = SYSTEM_DATABASES.contains(&name);
        if let Some(size) = int(d.get("sizeOnDisk")) {
            summary.extras.insert(props::SIZE_BYTES, size);
        }
        out.push(summary);
    }
    Ok(out)
}

/// `listCollections` entries of `db` matching `filter` (a filter on `name` / `type` only).
///
/// A user without the `listCollections` action on `db` (a role with collection-level
/// privileges only, like the fixture's `m7_role`) gets `Unauthorized` for the full listing;
/// then the listing is retried as `{nameOnly: true, authorizedCollections: true}`, which the
/// server allows and which lists the collections and views the user has privileges on (name and
/// type only: no options, so such entries show no capped flag or view source).
async fn collection_infos(
    conn: &MongoConnection,
    db: &str,
    filter: Document,
    cancel: &CancellationToken,
) -> Result<Vec<Document>> {
    let full = cursor_command(
        conn,
        db,
        doc! {"listCollections": 1_i32, "filter": filter.clone(), "nameOnly": false},
        cancel,
    )
    .await;
    match full {
        Err(e) if is_unauthorized(&e) => {
            cursor_command(
                conn,
                db,
                doc! {
                    "listCollections": 1_i32,
                    "filter": filter,
                    "nameOnly": true,
                    "authorizedCollections": true,
                },
                cancel,
            )
            .await
        }
        other => other,
    }
}

/// The server refused a command for missing privileges (`Unauthorized`, code 13).
fn is_unauthorized(e: &Error) -> bool {
    matches!(e, Error::Sql { code: Some(code), .. } if code == "Unauthorized" || code == "13")
}

async fn list_collections(
    conn: &MongoConnection,
    db: &str,
    cancel: &CancellationToken,
) -> Result<Vec<ObjectSummary>> {
    let infos = collection_infos(
        conn,
        db,
        doc! {"type": {"$in": ["collection", "timeseries"]}},
        cancel,
    )
    .await?;
    let mut out = Vec::with_capacity(infos.len());
    for info in &infos {
        let Ok(name) = info.get_str("name") else {
            continue;
        };
        let mut summary = ObjectSummary::new(ObjectRef::table(Some(db), None, name));
        summary.system = is_system_collection(name);
        let kind = info.get_str("type").unwrap_or("collection");
        summary
            .extras
            .insert(extras::COLLECTION_TYPE, kind.to_owned());
        if info
            .get_document("options")
            .is_ok_and(|o| o.get_bool("capped").unwrap_or(false))
        {
            summary.extras.insert(extras::CAPPED, true);
        }
        out.push(summary);
    }
    // Document counts (metadata only: `count` without a filter), a few at a time; a collection
    // the user may not count keeps no count.
    let comment = conn.next_comment();
    let database = conn.client().database(db);
    let names: Vec<String> = out.iter().map(|s| s.obj.name.clone()).collect();
    let counts: Vec<Option<u64>> = guarded(conn, &comment, cancel, async {
        Ok(futures::stream::iter(names)
            .map(|name| {
                let coll = database.collection::<Document>(&name);
                let comment = comment.clone();
                async move { coll.estimated_document_count().comment(comment).await.ok() }
            })
            .buffered(COUNT_CONCURRENCY)
            .collect()
            .await)
    })
    .await?;
    for (summary, count) in out.iter_mut().zip(counts) {
        if let Some(count) = count {
            summary.row_estimate = Some(count);
            summary.extras.insert(props::DOCUMENT_COUNT, count);
        }
    }
    Ok(out)
}

async fn list_views(
    conn: &MongoConnection,
    db: &str,
    cancel: &CancellationToken,
) -> Result<Vec<ObjectSummary>> {
    let infos = collection_infos(conn, db, doc! {"type": "view"}, cancel).await?;
    let mut out = Vec::with_capacity(infos.len());
    for info in &infos {
        let Ok(name) = info.get_str("name") else {
            continue;
        };
        let mut summary =
            ObjectSummary::new(ObjectRef::new(ObjectKind::View, name).in_catalog(Some(db)));
        summary.system = is_system_collection(name);
        if let Ok(on) = info
            .get_document("options")
            .and_then(|o| o.get_str("viewOn"))
        {
            summary.extras.insert(extras::VIEW_ON, on.to_owned());
        }
        out.push(summary);
    }
    Ok(out)
}

/// The `listIndexes` entries of `collection`; `NotFound` when it does not exist.
async fn index_infos(
    conn: &MongoConnection,
    collection: &ObjectRef,
    cancel: &CancellationToken,
) -> Result<Vec<Document>> {
    let db = database_of(conn, collection)?;
    cursor_command(
        conn,
        &db,
        doc! {"listIndexes": collection.name.as_str()},
        cancel,
    )
    .await
}

async fn list_indexes(
    conn: &MongoConnection,
    parent: &ObjectRef,
    cancel: &CancellationToken,
) -> Result<Vec<ObjectSummary>> {
    if parent.kind != ObjectKind::Table {
        // Views have no indexes of their own.
        return Ok(Vec::new());
    }
    let infos = index_infos(conn, parent, cancel).await?;
    Ok(infos
        .iter()
        .filter_map(|info| {
            let index = index_def(info)?;
            let mut summary = ObjectSummary::new(parent.child(ObjectKind::Index, &index.name));
            summary.extras = index.extras;
            Some(summary)
        })
        .collect())
}

async fn list_users(
    conn: &MongoConnection,
    db: &str,
    cancel: &CancellationToken,
) -> Result<Vec<ObjectSummary>> {
    let reply = command(conn, db, doc! {"usersInfo": 1_i32}, cancel).await?;
    let users = reply
        .get_array("users")
        .map_err(|_| Error::internal("usersInfo returned no user list"))?;
    Ok(users
        .iter()
        .filter_map(|u| match u {
            Bson::Document(u) => {
                let name = u.get_str("user").ok()?;
                let mut summary =
                    ObjectSummary::new(ObjectRef::new(ObjectKind::User, name).in_catalog(Some(db)));
                summary.extras = user_extras(u);
                Some(summary)
            }
            _ => None,
        })
        .collect())
}

async fn list_roles(
    conn: &MongoConnection,
    db: &str,
    cancel: &CancellationToken,
) -> Result<Vec<ObjectSummary>> {
    let reply = command(
        conn,
        db,
        doc! {"rolesInfo": 1_i32, "showBuiltinRoles": true},
        cancel,
    )
    .await?;
    let roles = reply
        .get_array("roles")
        .map_err(|_| Error::internal("rolesInfo returned no role list"))?;
    Ok(roles
        .iter()
        .filter_map(|r| match r {
            Bson::Document(r) => {
                let name = r.get_str("role").ok()?;
                let mut summary =
                    ObjectSummary::new(ObjectRef::new(ObjectKind::Role, name).in_catalog(Some(db)));
                summary.system = r.get_bool("isBuiltin").unwrap_or(false);
                if let Some(granted) = r.get("roles") {
                    summary.extras.insert(extras::ROLES, json(granted));
                }
                Some(summary)
            }
            _ => None,
        })
        .collect())
}

/// Roles and mechanisms of a `usersInfo` entry (never credentials).
fn user_extras(user: &Document) -> Extras {
    let mut out = Extras::new();
    if let Some(roles) = user.get("roles") {
        out.insert(extras::ROLES, json(roles));
    }
    if let Some(mechanisms) = user.get("mechanisms") {
        out.insert(extras::MECHANISMS, json(mechanisms));
    }
    out
}

/// An index of a `listIndexes` entry; `None` without a name or key.
fn index_def(info: &Document) -> Option<IndexDef> {
    let name = info.get_str("name").ok()?.to_owned();
    let key = info.get_document("key").ok()?;
    let mut columns = Vec::with_capacity(key.len());
    let mut method = None;
    for (field, direction) in key {
        let mut column = IndexColumn::column(field.clone());
        match direction {
            Bson::String(kind) => method = Some(kind.clone()),
            other => column.descending = int(Some(other)).is_some_and(|n| n < 0),
        }
        columns.push(column);
    }
    let primary = name == "_id_";
    let unique = primary || info.get_bool("unique").unwrap_or(false);
    let mut extras = Extras::new();
    extras.insert(extras::INDEX_KEYS, json(&Bson::Document(key.clone())));
    extras.insert(props::UNIQUE, unique);
    if primary {
        extras.insert(props::PRIMARY_KEY, true);
    }
    if let Some(method) = method {
        extras.insert(props::INDEX_METHOD, method);
    }
    if info.get_bool("sparse").unwrap_or(false) {
        extras.insert(extras::SPARSE, true);
    }
    if let Some(seconds) = int(info.get("expireAfterSeconds")) {
        extras.insert(extras::EXPIRE_AFTER_SECONDS, seconds);
    }
    if info.get_bool("hidden").unwrap_or(false) {
        extras.insert(extras::HIDDEN, true);
    }
    let predicate = info
        .get_document("partialFilterExpression")
        .ok()
        .map(|p| Bson::Document(p.clone()).into_relaxed_extjson().to_string());
    if let Some(predicate) = &predicate {
        extras.insert(props::PREDICATE, predicate.clone());
    }
    Some(IndexDef {
        name,
        columns,
        unique,
        is_constraint: false,
        predicate,
        extras,
    })
}

// ---- definitions --------------------------------------------------------------------------------

async fn load_database(
    conn: &MongoConnection,
    obj: &ObjectRef,
    cancel: &CancellationToken,
) -> Result<ObjectDef> {
    let db = database_of(conn, obj)?;
    let stats = command(conn, &db, doc! {"dbStats": 1_i32, "scale": 1_i32}, cancel).await?;
    let mut def = SchemaDef::new(ObjectRef::database(db));
    let size = int(stats.get("totalSize")).or_else(|| {
        Some(int(stats.get("storageSize"))? + int(stats.get("indexSize")).unwrap_or(0))
    });
    if let Some(size) = size {
        def.extras.insert(props::SIZE_BYTES, size);
    }
    if let Some(objects) = int(stats.get("objects")) {
        def.extras.insert(props::DOCUMENT_COUNT, objects);
    }
    Ok(ObjectDef::Schema(def))
}

/// The `listCollections` entry of `name` in `db` when it has one of `types`.
async fn collection_info(
    conn: &MongoConnection,
    db: &str,
    name: &str,
    types: &[&str],
    cancel: &CancellationToken,
) -> Result<Option<Document>> {
    let infos = collection_infos(conn, db, doc! {"name": name}, cancel).await?;
    Ok(infos
        .into_iter()
        .find(|info| types.contains(&info.get_str("type").unwrap_or("collection"))))
}

async fn load_collection(
    conn: &MongoConnection,
    obj: &ObjectRef,
    cancel: &CancellationToken,
) -> Result<ObjectDef> {
    let db = database_of(conn, obj)?;
    let info = collection_info(conn, &db, &obj.name, &["collection", "timeseries"], cancel)
        .await?
        .ok_or_else(|| {
            Error::not_found(format!("collection {}.{} does not exist", db, obj.name))
        })?;
    let options = info.get_document("options").cloned().unwrap_or_default();
    let reference = ObjectRef::table(Some(&db), None, &obj.name);
    let mut def = TableDef::new(reference.clone());

    let count = {
        let comment = conn.next_comment();
        let tag = comment.clone();
        let coll = conn
            .client()
            .database(&db)
            .collection::<Document>(&obj.name);
        guarded(conn, &comment, cancel, async move {
            coll.estimated_document_count()
                .comment(tag)
                .await
                .map_err(|e| map(&e))
        })
        .await?
    };
    let sample = sample(conn, &db, &obj.name, count, cancel).await?;
    def.columns = infer_columns(&sample);
    def.primary_key = Some(PrimaryKeyDef {
        name: Some("_id_".to_owned()),
        columns: vec!["_id".to_owned()],
    });
    let index_infos = index_infos(conn, &reference, cancel).await?;
    def.indexes = index_infos.iter().filter_map(index_def).collect();

    let kind = info.get_str("type").unwrap_or("collection");
    def.extras.insert(extras::COLLECTION_TYPE, kind.to_owned());
    def.extras
        .insert(extras::CAPPED, options.get_bool("capped").unwrap_or(false));
    if let Some(validator) = options.get("validator") {
        def.extras.insert(extras::VALIDATOR, json(validator));
    }
    def.extras
        .insert(extras::OPTIONS, json(&Bson::Document(options)));
    def.extras.insert(props::DOCUMENT_COUNT, count);
    if let Some(size) = collection_size(conn, &db, &obj.name, cancel).await? {
        def.extras.insert(props::SIZE_BYTES, size);
    }
    Ok(ObjectDef::Table(def))
}

/// The first `sample_size` documents (all of a small collection, in natural order), or a
/// `$sample` of that many from a larger one.
async fn sample(
    conn: &MongoConnection,
    db: &str,
    name: &str,
    count: u64,
    cancel: &CancellationToken,
) -> Result<Vec<Document>> {
    let size = conn.options().sample_size.max(1);
    let size_i64 = i64::try_from(size).unwrap_or(i64::MAX);
    let comment = conn.next_comment();
    let coll = conn.client().database(db).collection::<Document>(name);
    let tag = comment.clone();
    let small = usize::try_from(count).is_ok_and(|n| n <= size);
    guarded(conn, &comment, cancel, async move {
        let cursor = if small {
            coll.find(doc! {})
                .limit(size_i64)
                .batch_size(u32::try_from(size).unwrap_or(u32::MAX))
                .comment(tag)
                .await
        } else {
            coll.aggregate([doc! {"$sample": {"size": size_i64}}])
                .comment(tag)
                .await
        }
        .map_err(|e| map(&e))?;
        cursor.try_collect().await.map_err(|e| map(&e))
    })
    .await
}

/// Storage size of a collection (`$collStats` `storageStats.totalSize`, else storage + index
/// sizes); `None` when the server does not tell (permissions, views).
async fn collection_size(
    conn: &MongoConnection,
    db: &str,
    name: &str,
    cancel: &CancellationToken,
) -> Result<Option<i64>> {
    let comment = conn.next_comment();
    let tag = comment.clone();
    let coll = conn.client().database(db).collection::<Document>(name);
    let stats: Option<Document> = guarded(conn, &comment, cancel, async move {
        let Ok(cursor) = coll
            .aggregate([doc! {"$collStats": {"storageStats": {}}}])
            .comment(tag)
            .await
        else {
            return Ok(None);
        };
        let docs: Vec<Document> = cursor.try_collect().await.unwrap_or_default();
        Ok(docs.into_iter().next())
    })
    .await?;
    Ok(stats.and_then(|s| {
        let storage = s.get_document("storageStats").ok()?;
        int(storage.get("totalSize")).or_else(|| {
            Some(int(storage.get("storageSize"))? + int(storage.get("totalIndexSize")).unwrap_or(0))
        })
    }))
}

/// Columns of a collection from a sample: the document layout's columns (`_id` first, then by
/// first appearance), typed from the BSON types seen.
pub(crate) fn infer_columns(sample: &[Document]) -> Vec<ColumnDef> {
    let layout = DocumentLayout::infer(sample, None);
    let schema = layout.schema();
    let mut present: HashMap<&str, usize> = HashMap::new();
    for doc in sample {
        for (key, value) in doc {
            if !matches!(value, Bson::Null | Bson::Undefined) {
                *present.entry(key.as_str()).or_default() += 1;
            }
        }
    }
    let mut columns: Vec<ColumnDef> = Vec::with_capacity(schema.fields().len());
    for field in schema.fields() {
        if field.name() == DOCUMENT_COLUMN {
            continue;
        }
        let bson_type = field
            .metadata()
            .get(META_BSON_TYPE)
            .map_or("mixed", String::as_str);
        let subtype = field.metadata().get(META_BSON_SUBTYPE).map(String::as_str);
        let mut column = ColumnDef::new(
            field.name().clone(),
            column_type(bson_type, subtype, field.data_type()),
        );
        bson_type.clone_into(&mut column.native_type);
        column.nullable = present.get(field.name().as_str()).copied().unwrap_or(0) < sample.len();
        column
            .extras
            .insert(props::NATIVE_TYPE, bson_type.to_owned());
        columns.push(column);
    }
    if let Some(id) = columns.iter_mut().find(|c| c.name == "_id") {
        id.nullable = false;
        // The client gives a document without `_id` a new ObjectId.
        id.default = Some("ObjectId()".to_owned());
    } else {
        let mut id = ColumnDef::new(
            "_id",
            DataType::Other {
                name: "objectId".to_owned(),
            },
        );
        "objectId".clone_into(&mut id.native_type);
        id.nullable = false;
        id.default = Some("ObjectId()".to_owned());
        id.extras.insert(props::NATIVE_TYPE, "objectId");
        columns.insert(0, id);
    }
    columns
}

/// The logical type of a column whose values have `bson_type`.
fn column_type(bson_type: &str, subtype: Option<&str>, arrow: &ArrowType) -> DataType {
    match bson_type {
        "string" => DataType::Text,
        "int" => DataType::INT32,
        "long" => DataType::INT64,
        "double" => DataType::FLOAT64,
        "decimal" => DataType::Decimal {
            precision: None,
            scale: None,
        },
        "bool" => DataType::Bool,
        "date" => DataType::TimestampTz { precision: Some(3) },
        "binData" if subtype == Some("04") => DataType::Uuid,
        "binData" => DataType::VarBinary { length: None },
        "object" | "array" | "mixed" => DataType::Json { binary: true },
        _ if *arrow == ArrowType::Utf8 && bson_type != "objectId" => {
            DataType::Json { binary: true }
        }
        other => DataType::Other {
            name: other.to_owned(),
        },
    }
}

async fn load_view(
    conn: &MongoConnection,
    obj: &ObjectRef,
    cancel: &CancellationToken,
) -> Result<ObjectDef> {
    let db = database_of(conn, obj)?;
    let info = collection_info(conn, &db, &obj.name, &["view"], cancel)
        .await?
        .ok_or_else(|| Error::not_found(format!("view {}.{} does not exist", db, obj.name)))?;
    let options = info.get_document("options").cloned().unwrap_or_default();
    let pipeline = options
        .get("pipeline")
        .cloned()
        .unwrap_or_else(|| Bson::Array(Vec::new()));
    let mut extras = Extras::new();
    if let Ok(on) = options.get_str("viewOn") {
        extras.insert(extras::VIEW_ON, on.to_owned());
    }
    extras.insert(extras::OPTIONS, json(&Bson::Document(options.clone())));
    Ok(ObjectDef::View(ViewDef {
        obj: ObjectRef::new(ObjectKind::View, &obj.name).in_catalog(Some(&db)),
        definition: to_canonical_text(&pipeline),
        columns: Vec::new(),
        comment: None,
        extras,
    }))
}

async fn load_index(
    conn: &MongoConnection,
    obj: &ObjectRef,
    cancel: &CancellationToken,
) -> Result<ObjectDef> {
    let owner = obj
        .owner()
        .ok_or_else(|| Error::invalid_input("an index ref needs its collection (parent)"))?;
    let db = database_of(conn, obj)?;
    let table = ObjectRef::table(Some(&db), None, &owner.name);
    let infos = index_infos(conn, &table, cancel).await?;
    let index = infos
        .iter()
        .filter_map(index_def)
        .find(|i| i.name == obj.name)
        .ok_or_else(|| {
            Error::not_found(format!(
                "index {} of {}.{} does not exist",
                obj.name, db, owner.name
            ))
        })?;
    Ok(ObjectDef::Index { table, index })
}

async fn load_user(
    conn: &MongoConnection,
    obj: &ObjectRef,
    cancel: &CancellationToken,
) -> Result<ObjectDef> {
    let db = database_of(conn, obj)?;
    let reply = command(
        conn,
        &db,
        doc! {"usersInfo": {"user": obj.name.as_str(), "db": db.as_str()}},
        cancel,
    )
    .await?;
    let user = reply
        .get_array("users")
        .ok()
        .and_then(|users| users.first())
        .and_then(Bson::as_document)
        .ok_or_else(|| Error::not_found(format!("user {}@{} does not exist", obj.name, db)))?;
    Ok(ObjectDef::Generic(GenericDef {
        obj: ObjectRef::new(ObjectKind::User, &obj.name).in_catalog(Some(&db)),
        definition: None,
        comment: None,
        extras: user_extras(user),
    }))
}

async fn load_role(
    conn: &MongoConnection,
    obj: &ObjectRef,
    cancel: &CancellationToken,
) -> Result<ObjectDef> {
    let db = database_of(conn, obj)?;
    let reply = command(
        conn,
        &db,
        doc! {
            "rolesInfo": {"role": obj.name.as_str(), "db": db.as_str()},
            "showPrivileges": true,
            "showBuiltinRoles": true,
        },
        cancel,
    )
    .await?;
    let role = reply
        .get_array("roles")
        .ok()
        .and_then(|roles| roles.first())
        .and_then(Bson::as_document)
        .ok_or_else(|| Error::not_found(format!("role {}@{} does not exist", obj.name, db)))?;
    let mut extras = Extras::new();
    if let Some(granted) = role.get("roles") {
        extras.insert(extras::ROLES, json(granted));
    }
    if let Some(privileges) = role.get("privileges") {
        extras.insert(extras::PRIVILEGES, json(privileges));
    }
    Ok(ObjectDef::Generic(GenericDef {
        obj: ObjectRef::new(ObjectKind::Role, &obj.name).in_catalog(Some(&db)),
        definition: None,
        comment: None,
        extras,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::oid::ObjectId;
    use bson::spec::BinarySubtype;
    use bson::{Binary, DateTime, Decimal128};

    #[test]
    fn columns_follow_the_sample_with_id_first() {
        let sample = vec![
            doc! {"name": "a", "_id": ObjectId::new(), "n": 1_i32, "price": "1.5".parse::<Decimal128>().unwrap()},
            doc! {"_id": ObjectId::new(), "n": 2_i64, "at": DateTime::from_millis(0), "mixed": "x",
            "uuid": Binary {subtype: BinarySubtype::Uuid, bytes: vec![0; 16]}},
            doc! {"_id": ObjectId::new(), "name": "c", "mixed": 3_i32, "meta": {"a": 1_i32}, "tags": ["x"],
            "flag": true, "bin": Binary {subtype: BinarySubtype::Generic, bytes: vec![1]}},
        ];
        let columns = infer_columns(&sample);
        let shape: Vec<(&str, &DataType, &str, bool)> = columns
            .iter()
            .map(|c| {
                (
                    c.name.as_str(),
                    &c.data_type,
                    c.native_type.as_str(),
                    c.nullable,
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                (
                    "_id",
                    &DataType::Other {
                        name: "objectId".into()
                    },
                    "objectId",
                    false
                ),
                ("name", &DataType::Text, "string", true),
                ("n", &DataType::INT64, "long", true),
                (
                    "price",
                    &DataType::Decimal {
                        precision: None,
                        scale: None
                    },
                    "decimal",
                    true
                ),
                (
                    "at",
                    &DataType::TimestampTz { precision: Some(3) },
                    "date",
                    true
                ),
                ("mixed", &DataType::Json { binary: true }, "mixed", true),
                ("uuid", &DataType::Uuid, "binData", true),
                ("meta", &DataType::Json { binary: true }, "object", true),
                ("tags", &DataType::Json { binary: true }, "array", true),
                ("flag", &DataType::Bool, "bool", true),
                (
                    "bin",
                    &DataType::VarBinary { length: None },
                    "binData",
                    true
                ),
            ]
        );
        assert_eq!(columns[0].default.as_deref(), Some("ObjectId()"));
    }

    #[test]
    fn only_unauthorized_errors_retry_the_authorized_listing() {
        assert!(is_unauthorized(&Error::sql(
            Some("Unauthorized"),
            "not authorized"
        )));
        assert!(is_unauthorized(&Error::sql(Some("13"), "not authorized")));
        assert!(!is_unauthorized(&Error::sql(
            Some("NamespaceNotFound"),
            "x"
        )));
        assert!(!is_unauthorized(&Error::sql(None::<String>, "x")));
        assert!(!is_unauthorized(&Error::Cancelled));
    }

    #[test]
    fn an_empty_collection_still_has_its_id_column() {
        let columns = infer_columns(&[]);
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "_id");
        assert!(!columns[0].nullable);
    }

    #[test]
    fn index_entries_become_index_defs() {
        let primary = index_def(&doc! {"v": 2_i32, "key": {"_id": 1_i32}, "name": "_id_"}).unwrap();
        assert!(primary.unique);
        assert_eq!(
            primary.extras.get(&props::PRIMARY_KEY),
            Some(&Value::Bool(true))
        );

        let partial = index_def(&doc! {
            "v": 2_i32, "key": {"meta.city": 1_i32, "at": -1.0}, "name": "uq", "unique": true,
            "partialFilterExpression": {"meta.city": {"$exists": true}},
        })
        .unwrap();
        let cols: Vec<(&str, bool)> = partial
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.descending))
            .collect();
        assert_eq!(cols, [("meta.city", false), ("at", true)]);
        assert!(partial.unique);
        assert_eq!(
            partial.predicate.as_deref(),
            Some(r#"{"meta.city":{"$exists":true}}"#)
        );

        let ttl = index_def(&doc! {"key": {"at": 1_i32}, "name": "ttl", "expireAfterSeconds": 60_i32, "sparse": true})
            .unwrap();
        assert_eq!(
            ttl.extras.get(&extras::EXPIRE_AFTER_SECONDS),
            Some(&Value::Int(60))
        );
        assert_eq!(ttl.extras.get(&extras::SPARSE), Some(&Value::Bool(true)));
        assert!(!ttl.unique);

        let text = index_def(&doc! {"key": {"_fts": "text", "_ftsx": 1_i32}, "name": "t"}).unwrap();
        assert_eq!(
            text.extras.get(&props::INDEX_METHOD),
            Some(&Value::Text("text".into()))
        );
        assert!(index_def(&doc! {"name": "no key"}).is_none());
    }

    #[test]
    fn reply_numbers_read_as_integers() {
        assert_eq!(int(Some(&Bson::Int32(3))), Some(3));
        assert_eq!(int(Some(&Bson::Int64(4))), Some(4));
        assert_eq!(int(Some(&Bson::Double(4096.0))), Some(4096));
        assert_eq!(int(Some(&Bson::String("1".into()))), None);
        assert_eq!(int(None), None);
    }
}
