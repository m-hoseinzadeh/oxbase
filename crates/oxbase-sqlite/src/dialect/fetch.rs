//! The SQLite [`Fetcher`] cells: databases, tables, columns, indexes, foreign keys, checks,
//! views and triggers.
//!
//! Every cell reads through [`Catalog`] (bound parameters only, see its docs), checks the
//! cancellation token between round trips and returns `Cancelled` with nothing partial.
//! Facts the pragmas report win; the stored `CREATE` text ([`parse`]) adds what they do not
//! report (constraint names, check and generated expressions, collations, `AUTOINCREMENT`,
//! index expressions and predicates, view bodies, trigger clauses).
//!
//! **Unnamed constraints.** SQLite does not name a foreign key or check declared without
//! `CONSTRAINT <name>`. So that the tree can still address them, listings call such a
//! constraint `(foreign key n)` / `(check n)`, where `n` is its 1-based position among the
//! table's foreign keys / checks in declaration order, and mark the row with
//! [`UNNAMED`](super::UNNAMED); `load` accepts those names. The loaded definition keeps
//! `name: None`.
//!
//! **`TEMP` triggers.** A `TEMP` trigger is stored in `temp` but may fire on a table of any
//! database. It is listed (marked [`props::TEMPORARY`]) under the table it fires on, and `load`
//! finds it from that table's database too.
//!
//! **Unreadable owners.** Views and virtual tables have no indexes or foreign keys, so those
//! listings are empty without asking the pragmas (which fail on a virtual table whose module is
//! not loaded). When a column pragma fails on an existing owner (a view whose query no longer
//! resolves), the SQL error names the owner.

use std::collections::HashMap;

use async_trait::async_trait;
use oxbase_dialect::Fetcher;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    CheckDef, ColumnDef, Error, Extras, FkAction, ForeignKeyDef, GeneratedColumn, IndexColumn,
    IndexDef, ObjectDef, ObjectKind, ObjectRef, ObjectSummary, PrimaryKeyDef, Result, SchemaDef,
    TableDef, TriggerDef, TriggerEvent, TriggerLevel, TriggerTiming, ViewDef, props,
};

use super::catalog::{
    Catalog, ColumnRow, ForeignKeyRow, IndexKeyRow, IndexRow, SchemaRow, TableFlags, check,
};
use super::parse::{self, ParsedEvent, ParsedIndex, ParsedTable, ParsedTiming};
use super::types;
use super::{AUTOINCREMENT, COLUMN, HIDDEN, MODULE, REBUILD_LOSES, UNNAMED};

/// The database (SQLite schema name) `r` lives in: its catalog, or `main`.
fn schema_of(r: &ObjectRef) -> &str {
    r.database_name()
        .filter(|s| !s.is_empty())
        .unwrap_or("main")
}

fn is_temp(schema: &str) -> bool {
    schema.eq_ignore_ascii_case("temp")
}

/// `sqlite_*` names are reserved for SQLite's own objects.
fn is_system_name(name: &str) -> bool {
    name.get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("sqlite_"))
}

fn not_found(kind: ObjectKind, r: &ObjectRef) -> Error {
    Error::not_found(format!(
        "{} {} does not exist",
        kind.display_name(),
        r.path().join(".")
    ))
}

/// The owning table or view of a table child.
fn owner_ref(obj: &ObjectRef) -> Result<ObjectRef> {
    obj.owner().ok_or_else(|| {
        Error::invalid_input(format!(
            "{} {} names no owning table",
            obj.kind.display_name(),
            obj.name
        ))
    })
}

/// A table or view as `sqlite_schema` records it.
struct Owner {
    schema: String,
    row: SchemaRow,
    kind: ObjectKind,
}

impl Owner {
    /// Looks up the table or view `r` (`NotFound` when gone). `kinds` restricts the types.
    async fn find(cat: &Catalog<'_>, r: &ObjectRef, kinds: &[&str]) -> Result<Self> {
        let schema = schema_of(r);
        let row = cat
            .object(schema, &r.name, kinds)
            .await?
            .ok_or_else(|| not_found(r.kind, r))?;
        let kind = if row.kind == "view" {
            ObjectKind::View
        } else {
            ObjectKind::Table
        };
        Ok(Self {
            schema: schema.to_owned(),
            row,
            kind,
        })
    }

    /// A table or view.
    async fn any(cat: &Catalog<'_>, r: &ObjectRef) -> Result<Self> {
        Self::find(cat, r, &["table", "view"]).await
    }

    /// What the `CREATE TABLE` text says (nothing for views).
    fn parsed(&self) -> ParsedTable {
        match (&self.kind, &self.row.sql) {
            (ObjectKind::Table, Some(sql)) => parse::parse_create_table(sql),
            _ => ParsedTable::default(),
        }
    }

    fn name(&self) -> &str {
        &self.row.name
    }

    /// `true` for owners that cannot have indexes or foreign keys: views and virtual tables.
    /// Their listings are empty without asking the pragmas, which fail on a virtual table whose
    /// module is not loaded in this session.
    fn keyless(&self) -> bool {
        self.kind == ObjectKind::View || self.parsed().module.is_some()
    }

    /// Adds which owner a failed catalog pragma was reading. On an owner that exists, the
    /// pragmas fail when a virtual table's module is not loaded (`no such module`) or a view's
    /// query no longer resolves (`no such table`).
    fn context(&self, err: Error) -> Error {
        match err {
            Error::Sql { code, message } => Error::sql(
                code,
                format!(
                    "{} {}.{} cannot be read: {message}",
                    self.kind.display_name(),
                    self.schema,
                    self.name()
                ),
            ),
            other => other,
        }
    }

    /// Columns as `pragma_table_xinfo` reports them.
    async fn column_rows(&self, cat: &Catalog<'_>) -> Result<Vec<ColumnRow>> {
        cat.columns(&self.schema, self.name())
            .await
            .map_err(|e| self.context(e))
    }
}

// --- columns -------------------------------------------------------------------------------

fn build_columns(
    rows: &[ColumnRow],
    parsed: &ParsedTable,
    without_rowid: bool,
) -> (Vec<ColumnDef>, Option<PrimaryKeyDef>) {
    let mut columns = Vec::with_capacity(rows.len());
    let mut pk: Vec<(i64, String)> = Vec::new();
    for r in rows {
        let p = parsed
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(&r.name));
        let mut col = ColumnDef::new(r.name.clone(), types::data_type(&r.decl));
        col.native_type.clone_from(&r.decl);
        col.nullable = !r.not_null;
        col.default.clone_from(&r.default);
        col.collation = p.and_then(|p| p.collation.clone());
        if matches!(r.hidden, 2 | 3) {
            col.generated = p
                .and_then(|p| p.generated.clone())
                .map(|expression| GeneratedColumn {
                    expression,
                    stored: r.hidden == 3,
                });
        }
        if r.hidden == 1 {
            col.extras.insert(HIDDEN, true);
        }
        if p.is_some_and(|p| p.autoincrement) {
            col.auto_increment = true;
            col.extras.insert(AUTOINCREMENT, true);
        }
        if r.pk > 0 {
            pk.push((r.pk, r.name.clone()));
            // WITHOUT ROWID tables enforce NOT NULL on their key.
            if without_rowid {
                col.nullable = false;
            }
        }
        columns.push(col);
    }
    pk.sort();
    if !without_rowid
        && let [(_, only)] = pk.as_slice()
        && let Some(col) = columns.iter_mut().find(|c| c.name == *only)
        && types::is_rowid_alias_type(&col.native_type)
        // A column-level `INTEGER PRIMARY KEY DESC` is SQLite's documented exception.
        && !parsed
            .columns
            .iter()
            .any(|p| p.pk_desc && p.name.eq_ignore_ascii_case(only))
    {
        // `INTEGER PRIMARY KEY` is the rowid: never NULL, filled in when not given.
        col.auto_increment = true;
        col.nullable = false;
    }
    let primary_key = (!pk.is_empty()).then(|| PrimaryKeyDef {
        name: parsed.pk_name.clone(),
        columns: pk.into_iter().map(|(_, n)| n).collect(),
    });
    (columns, primary_key)
}

async fn owner_columns(
    cat: &Catalog<'_>,
    owner: &Owner,
) -> Result<(Vec<ColumnDef>, Option<PrimaryKeyDef>)> {
    let rows = owner.column_rows(cat).await?;
    let parsed = owner.parsed();
    Ok(build_columns(&rows, &parsed, parsed.without_rowid))
}

// --- indexes -------------------------------------------------------------------------------

fn index_def(row: &IndexRow, keys: &[IndexKeyRow], parsed: &ParsedIndex) -> IndexDef {
    let columns = keys
        .iter()
        .enumerate()
        .map(|(i, k)| {
            let (name, is_expression) = match k.cid {
                -2 => (parsed.parts.get(i).cloned().unwrap_or_default(), true),
                -1 => (k.name.clone().unwrap_or_else(|| "rowid".to_owned()), false),
                _ => (k.name.clone().unwrap_or_default(), false),
            };
            IndexColumn {
                name,
                is_expression,
                descending: k.desc,
                length: None,
            }
        })
        .collect();
    let mut extras = Extras::new();
    if row.origin == "pk" {
        extras.insert(props::PRIMARY_KEY, true);
    }
    if parsed.collated {
        extras.insert(REBUILD_LOSES, "a COLLATE on an index key");
    }
    IndexDef {
        name: row.name.clone(),
        columns,
        unique: row.unique,
        is_constraint: row.origin == "u",
        predicate: if row.partial {
            parsed.predicate.clone()
        } else {
            None
        },
        extras,
    }
}

/// Every index of `owner`, primary-key ones included (marked `PRIMARY_KEY`), sorted by name.
async fn index_defs(cat: &Catalog<'_>, owner: &Owner) -> Result<Vec<IndexDef>> {
    if owner.keyless() {
        return Ok(Vec::new());
    }
    let list = cat
        .indexes(&owner.schema, owner.name())
        .await
        .map_err(|e| owner.context(e))?;
    if list.is_empty() {
        return Ok(Vec::new());
    }
    let sqls: HashMap<String, String> = cat
        .objects_on(&owner.schema, "index", owner.name())
        .await?
        .into_iter()
        .filter_map(|r| Some((r.name.to_ascii_lowercase(), r.sql?)))
        .collect();
    let mut out = Vec::with_capacity(list.len());
    for row in &list {
        let keys = cat
            .index_keys(&owner.schema, &row.name)
            .await
            .map_err(|e| owner.context(e))?;
        let parsed = sqls
            .get(&row.name.to_ascii_lowercase())
            .map(|sql| parse::parse_create_index(sql.as_str()))
            .unwrap_or_default();
        out.push(index_def(row, &keys, &parsed));
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

// --- foreign keys and checks ---------------------------------------------------------------

/// A foreign key with its position among the table's foreign keys (1-based, declaration order).
struct ForeignKeyInfo {
    def: ForeignKeyDef,
    ordinal: usize,
}

impl ForeignKeyInfo {
    fn label(&self) -> String {
        self.def
            .name
            .clone()
            .unwrap_or_else(|| format!("(foreign key {})", self.ordinal))
    }
}

fn fk_action(text: &str) -> FkAction {
    match text.trim().to_ascii_uppercase().as_str() {
        "CASCADE" => FkAction::Cascade,
        "RESTRICT" => FkAction::Restrict,
        "SET NULL" => FkAction::SetNull,
        "SET DEFAULT" => FkAction::SetDefault,
        _ => FkAction::NoAction,
    }
}

fn same_names(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// The foreign keys of `owner` in declaration order. `owner_ref` gives the catalog of the
/// referenced tables (SQLite foreign keys never leave their database).
async fn foreign_key_defs(
    cat: &Catalog<'_>,
    owner: &Owner,
    owner_ref: &ObjectRef,
) -> Result<Vec<ForeignKeyInfo>> {
    if owner.keyless() {
        return Ok(Vec::new());
    }
    let rows = cat
        .foreign_keys(&owner.schema, owner.name())
        .await
        .map_err(|e| owner.context(e))?;
    let mut groups: Vec<Vec<&ForeignKeyRow>> = Vec::new();
    for r in &rows {
        match groups.last_mut() {
            Some(g) if g[0].id == r.id => g.push(r),
            _ => groups.push(vec![r]),
        }
    }
    let parsed = owner.parsed();
    let mut used = vec![false; parsed.foreign_keys.len()];
    let mut ref_pks: HashMap<String, Vec<String>> = HashMap::new();
    let mut out = Vec::with_capacity(groups.len());
    for (k, group) in groups.iter().enumerate() {
        let first = group[0];
        let columns: Vec<String> = group.iter().map(|r| r.from.clone()).collect();
        let mut ref_columns: Vec<String> = group.iter().filter_map(|r| r.to.clone()).collect();
        if ref_columns.len() != group.len() {
            // `REFERENCES parent` without columns: the parent's primary key.
            let key = first.table.to_ascii_lowercase();
            ref_columns = if let Some(pk) = ref_pks.get(&key) {
                pk.clone()
            } else {
                let mut pk: Vec<(i64, String)> = cat
                    .columns(&owner.schema, &first.table)
                    .await?
                    .into_iter()
                    .filter(|c| c.pk > 0)
                    .map(|c| (c.pk, c.name))
                    .collect();
                pk.sort();
                let pk: Vec<String> = pk.into_iter().map(|(_, n)| n).collect();
                ref_pks.insert(key, pk.clone());
                pk
            };
        }
        // Match the declaration: same columns and parent table, else the last unused one
        // (SQLite numbers foreign keys from the last declared).
        let pick = (0..used.len())
            .find(|&j| {
                let p = &parsed.foreign_keys[j];
                !used[j]
                    && p.ref_table.eq_ignore_ascii_case(&first.table)
                    && same_names(&p.columns, &columns)
                    && (p.ref_columns.is_empty() || same_names(&p.ref_columns, &ref_columns))
            })
            .or_else(|| (0..used.len()).rev().find(|&j| !used[j]));
        let declared = pick.map(|j| {
            used[j] = true;
            &parsed.foreign_keys[j]
        });
        let extras = Extras::new()
            .with(props::DEFERRABLE, declared.is_some_and(|p| p.deferrable))
            .with(
                props::INITIALLY_DEFERRED,
                declared.is_some_and(|p| p.initially_deferred),
            );
        out.push(ForeignKeyInfo {
            def: ForeignKeyDef {
                name: declared.and_then(|p| p.name.clone()),
                columns,
                ref_table: ObjectRef::table(
                    owner_ref.catalog.as_deref(),
                    None,
                    first.table.clone(),
                ),
                ref_columns,
                on_delete: fk_action(&first.on_delete),
                on_update: fk_action(&first.on_update),
                extras,
            },
            ordinal: pick.map_or(used.len() + k + 1, |j| j + 1),
        });
    }
    out.sort_by_key(|f| f.ordinal);
    Ok(out)
}

/// The checks of `parsed` with their labels (name, or `(check n)` in declaration order).
fn check_defs(parsed: &ParsedTable) -> Vec<(CheckDef, String)> {
    parsed
        .checks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let label = c
                .name
                .clone()
                .unwrap_or_else(|| format!("(check {})", i + 1));
            let mut extras = Extras::new();
            if let Some(column) = &c.column {
                extras.insert(COLUMN, column.clone());
            }
            (
                CheckDef {
                    name: c.name.clone(),
                    expression: c.expression.clone(),
                    extras,
                },
                label,
            )
        })
        .collect()
}

// --- tables --------------------------------------------------------------------------------

fn table_extras(schema: &str, flags: Option<&TableFlags>, parsed: &ParsedTable) -> Extras {
    let mut e = Extras::new();
    if parsed.module.is_some() || flags.is_some_and(|f| f.kind == "virtual") {
        e.insert(props::TABLE_TYPE, "virtual");
    } else if flags.is_some_and(|f| f.kind == "shadow") {
        e.insert(props::TABLE_TYPE, "shadow");
    }
    if let Some(module) = &parsed.module {
        e.insert(MODULE, module.clone());
    }
    if parsed.without_rowid || flags.is_some_and(|f| f.without_rowid) {
        e.insert(props::WITHOUT_ROWID, true);
    }
    if parsed.strict || flags.is_some_and(|f| f.strict) {
        e.insert(props::STRICT, true);
    }
    if !parsed.lossy.is_empty() {
        e.insert(REBUILD_LOSES, parsed.lossy.join(", "));
    }
    if is_temp(schema) {
        e.insert(props::TEMPORARY, true);
    }
    e
}

/// `pragma_table_list` flags by lower-cased name. They only add to what the `CREATE` text says
/// (shadow tables, mostly), so a failing `pragma_table_list` (it resolves every view and virtual
/// table of the database, and one broken view or unloaded module fails it) yields no flags
/// instead of failing the listing. Cancellation still fails.
async fn flags_by_name(cat: &Catalog<'_>, schema: &str) -> Result<HashMap<String, TableFlags>> {
    match cat.table_flags(schema).await {
        Ok(rows) => Ok(rows.into_iter().collect()),
        Err(Error::Sql { .. }) => Ok(HashMap::new()),
        Err(e) => Err(e),
    }
}

// --- the cells -----------------------------------------------------------------------------

/// `Database`: `main`, attached databases and `temp` (when it holds objects).
pub(crate) struct DatabaseFetcher;

#[async_trait]
impl Fetcher for DatabaseFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        _parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let cat = Catalog::new(c, &cancel);
        let mut out: Vec<ObjectSummary> = cat
            .databases()
            .await?
            .into_iter()
            .map(|d| {
                let mut s = ObjectSummary::new(ObjectRef::database(d.name.clone()));
                s.extras
                    .insert(props::CURRENT, d.name.eq_ignore_ascii_case("main"));
                if !d.file.is_empty() {
                    s.extras.insert(props::FILE, d.file);
                }
                if is_temp(&d.name) {
                    s.extras.insert(props::TEMPORARY, true);
                }
                s
            })
            .collect();
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
        let cat = Catalog::new(c, &cancel);
        let name = obj.database_name().unwrap_or_default();
        let db = cat
            .database(name)
            .await?
            .filter(|_| !name.is_empty())
            .ok_or_else(|| not_found(ObjectKind::Database, obj))?;
        let mut def = SchemaDef::new(obj.clone());
        if !db.file.is_empty() {
            def.extras.insert(props::FILE, db.file);
        }
        if let Some(encoding) = cat.encoding().await? {
            def.extras.insert(props::ENCODING, encoding);
        }
        if is_temp(&db.name) {
            def.extras.insert(props::TEMPORARY, true);
        }
        check(&cancel)?;
        Ok(ObjectDef::Schema(def))
    }
}

/// `Table`: tables of a database (`sqlite_*` and virtual-table shadow tables are `system`).
pub(crate) struct TableFetcher;

#[async_trait]
impl Fetcher for TableFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let cat = Catalog::new(c, &cancel);
        let schema = schema_of(parent);
        let flags = flags_by_name(&cat, schema).await?;
        let rows = cat.objects(schema, "table").await?;
        let out = rows
            .into_iter()
            .map(|row| {
                let f = flags.get(&row.name.to_ascii_lowercase());
                let parsed = row
                    .sql
                    .as_deref()
                    .map(parse::parse_create_table)
                    .unwrap_or_default();
                let mut s = ObjectSummary::new(parent.child(ObjectKind::Table, row.name.clone()));
                s.system = is_system_name(&row.name) || f.is_some_and(|f| f.kind == "shadow");
                s.extras = table_extras(schema, f, &parsed);
                s
            })
            .collect();
        check(&cancel)?;
        Ok(out)
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        let cat = Catalog::new(c, &cancel);
        let owner = Owner::find(&cat, obj, &["table"]).await?;
        let parsed = owner.parsed();
        let flags = flags_by_name(&cat, &owner.schema).await?;
        let flags = flags.get(&owner.name().to_ascii_lowercase());
        let rows = owner.column_rows(&cat).await?;
        let without_rowid = parsed.without_rowid || flags.is_some_and(|f| f.without_rowid);
        let (columns, primary_key) = build_columns(&rows, &parsed, without_rowid);
        let indexes = index_defs(&cat, &owner)
            .await?
            .into_iter()
            .filter(|i| !i.extras.contains(&props::PRIMARY_KEY))
            .collect();
        let foreign_keys = foreign_key_defs(&cat, &owner, obj)
            .await?
            .into_iter()
            .map(|f| f.def)
            .collect();
        let mut def = TableDef::new(obj.clone());
        def.columns = columns;
        def.primary_key = primary_key;
        def.indexes = indexes;
        def.foreign_keys = foreign_keys;
        def.checks = check_defs(&parsed).into_iter().map(|(c, _)| c).collect();
        def.extras = table_extras(&owner.schema, flags, &parsed);
        check(&cancel)?;
        Ok(ObjectDef::Table(def))
    }
}

/// `View`: views of a database.
pub(crate) struct ViewFetcher;

#[async_trait]
impl Fetcher for ViewFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let cat = Catalog::new(c, &cancel);
        let schema = schema_of(parent);
        let out = cat
            .objects(schema, "view")
            .await?
            .into_iter()
            .map(|row| {
                let mut s = ObjectSummary::new(parent.child(ObjectKind::View, row.name.clone()));
                s.system = is_system_name(&row.name);
                if is_temp(schema) {
                    s.extras.insert(props::TEMPORARY, true);
                }
                s
            })
            .collect();
        check(&cancel)?;
        Ok(out)
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        let cat = Catalog::new(c, &cancel);
        let owner = Owner::find(&cat, obj, &["view"]).await?;
        let sql = owner.row.sql.clone().unwrap_or_default();
        let parsed = parse::parse_create_view(&sql);
        let mut extras = Extras::new();
        if is_temp(&owner.schema) {
            extras.insert(props::TEMPORARY, true);
        }
        check(&cancel)?;
        Ok(ObjectDef::View(ViewDef {
            obj: obj.clone(),
            definition: if parsed.definition.is_empty() {
                sql
            } else {
                parsed.definition
            },
            columns: parsed.columns,
            comment: None,
            extras,
        }))
    }
}

/// `Column`: columns of a table or view, in ordinal order.
pub(crate) struct ColumnFetcher;

#[async_trait]
impl Fetcher for ColumnFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let cat = Catalog::new(c, &cancel);
        let owner = Owner::any(&cat, parent).await?;
        let out = owner
            .column_rows(&cat)
            .await?
            .into_iter()
            .map(|col| {
                let mut s = ObjectSummary::new(parent.child(ObjectKind::Column, col.name.clone()));
                s.system = col.hidden == 1;
                s.extras.insert(props::NATIVE_TYPE, col.decl);
                s.extras.insert(props::PRIMARY_KEY, col.pk > 0);
                s
            })
            .collect();
        check(&cancel)?;
        Ok(out)
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        let cat = Catalog::new(c, &cancel);
        let table = owner_ref(obj)?;
        let owner = Owner::any(&cat, &table).await?;
        let (columns, _) = owner_columns(&cat, &owner).await?;
        let column = columns
            .into_iter()
            .find(|col| col.name.eq_ignore_ascii_case(&obj.name))
            .ok_or_else(|| not_found(ObjectKind::Column, obj))?;
        check(&cancel)?;
        Ok(ObjectDef::Column { table, column })
    }
}

/// `Index`: indexes of a table, the primary-key index included (marked `PRIMARY_KEY`).
pub(crate) struct IndexFetcher;

#[async_trait]
impl Fetcher for IndexFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let cat = Catalog::new(c, &cancel);
        let owner = Owner::any(&cat, parent).await?;
        let rows = if owner.keyless() {
            Vec::new()
        } else {
            cat.indexes(&owner.schema, owner.name())
                .await
                .map_err(|e| owner.context(e))?
        };
        let mut out: Vec<ObjectSummary> = rows
            .into_iter()
            .map(|row| {
                let mut s = ObjectSummary::new(parent.child(ObjectKind::Index, row.name));
                s.extras.insert(props::PRIMARY_KEY, row.origin == "pk");
                s.extras.insert(props::UNIQUE, row.unique);
                s
            })
            .collect();
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
        let cat = Catalog::new(c, &cancel);
        let table = if let Some(t) = obj.owner() {
            t
        } else {
            // An index ref without its table: find the table in `sqlite_schema`.
            let row = cat
                .object(schema_of(obj), &obj.name, &["index"])
                .await?
                .ok_or_else(|| not_found(ObjectKind::Index, obj))?;
            ObjectRef::table(obj.catalog.as_deref(), None, row.tbl_name)
        };
        let owner = Owner::any(&cat, &table).await?;
        let index = index_defs(&cat, &owner)
            .await?
            .into_iter()
            .find(|i| i.name.eq_ignore_ascii_case(&obj.name))
            .ok_or_else(|| not_found(ObjectKind::Index, obj))?;
        check(&cancel)?;
        Ok(ObjectDef::Index { table, index })
    }
}

/// `ForeignKey`: foreign keys of a table (see the module docs for unnamed ones).
pub(crate) struct ForeignKeyFetcher;

#[async_trait]
impl Fetcher for ForeignKeyFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let cat = Catalog::new(c, &cancel);
        let owner = Owner::any(&cat, parent).await?;
        let mut out: Vec<ObjectSummary> = foreign_key_defs(&cat, &owner, parent)
            .await?
            .into_iter()
            .map(|f| {
                let mut s = ObjectSummary::new(parent.child(ObjectKind::ForeignKey, f.label()));
                if f.def.name.is_none() {
                    s.extras.insert(UNNAMED, true);
                }
                s
            })
            .collect();
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
        let cat = Catalog::new(c, &cancel);
        let table = owner_ref(obj)?;
        let owner = Owner::any(&cat, &table).await?;
        let fks = foreign_key_defs(&cat, &owner, &table).await?;
        let found = fks
            .iter()
            .position(|f| {
                f.def
                    .name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(&obj.name))
            })
            .or_else(|| {
                fks.iter()
                    .position(|f| f.def.name.is_none() && f.label() == obj.name)
            })
            .ok_or_else(|| not_found(ObjectKind::ForeignKey, obj))?;
        let foreign_key = fks
            .into_iter()
            .nth(found)
            .map(|f| f.def)
            .ok_or_else(|| not_found(ObjectKind::ForeignKey, obj))?;
        check(&cancel)?;
        Ok(ObjectDef::ForeignKey { table, foreign_key })
    }
}

/// `Check`: check constraints of a table, read from its `CREATE TABLE` text.
pub(crate) struct CheckFetcher;

#[async_trait]
impl Fetcher for CheckFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let cat = Catalog::new(c, &cancel);
        let owner = Owner::any(&cat, parent).await?;
        let mut out: Vec<ObjectSummary> = check_defs(&owner.parsed())
            .into_iter()
            .map(|(def, label)| {
                let mut s = ObjectSummary::new(parent.child(ObjectKind::Check, label));
                if def.name.is_none() {
                    s.extras.insert(UNNAMED, true);
                }
                s
            })
            .collect();
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
        let cat = Catalog::new(c, &cancel);
        let table = owner_ref(obj)?;
        let owner = Owner::any(&cat, &table).await?;
        let checks = check_defs(&owner.parsed());
        let found = checks
            .iter()
            .position(|(def, _)| {
                def.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(&obj.name))
            })
            .or_else(|| {
                checks
                    .iter()
                    .position(|(def, label)| def.name.is_none() && *label == obj.name)
            })
            .ok_or_else(|| not_found(ObjectKind::Check, obj))?;
        let check_def = checks
            .into_iter()
            .nth(found)
            .map(|(def, _)| def)
            .ok_or_else(|| not_found(ObjectKind::Check, obj))?;
        check(&cancel)?;
        Ok(ObjectDef::Check {
            table,
            check: check_def,
        })
    }
}

/// `Trigger`: triggers of a table or view.
pub(crate) struct TriggerFetcher;

fn trigger_def(obj: ObjectRef, row: &SchemaRow, schema: &str) -> Result<TriggerDef> {
    let sql = row.sql.clone().unwrap_or_default();
    let p = parse::parse_create_trigger(&sql).ok_or_else(|| {
        Error::internal(format!(
            "cannot read the definition of trigger {}",
            row.name
        ))
    })?;
    let mut extras = Extras::new();
    if is_temp(schema) {
        extras.insert(props::TEMPORARY, true);
    }
    Ok(TriggerDef {
        obj,
        timing: match p.timing {
            ParsedTiming::Before => TriggerTiming::Before,
            ParsedTiming::After => TriggerTiming::After,
            ParsedTiming::InsteadOf => TriggerTiming::InsteadOf,
        },
        events: vec![match p.event {
            ParsedEvent::Insert => TriggerEvent::Insert,
            ParsedEvent::Update => TriggerEvent::Update,
            ParsedEvent::Delete => TriggerEvent::Delete,
        }],
        update_columns: p.update_columns,
        level: TriggerLevel::Row,
        condition: p.condition,
        function: None,
        body: Some(p.body),
        definition: Some(sql),
        enabled: true,
        comment: None,
        extras,
    })
}

/// The database holding the table or view a trigger stored in `temp` fires on.
///
/// A `TEMP` trigger may fire on a table of any database, and `temp.sqlite_schema` only records
/// the table's name. SQLite binds it to the database written in the `ON` clause (`ON main.t`)
/// or, without one, to the first database holding a table or view of that name in its search
/// order: `temp`, `main`, then the attached ones. `None` when no database holds it.
async fn temp_trigger_target(cat: &Catalog<'_>, row: &SchemaRow) -> Result<Option<String>> {
    if let Some(schema) = row
        .sql
        .as_deref()
        .and_then(parse::parse_create_trigger)
        .and_then(|p| p.table_schema)
    {
        return Ok(Some(schema));
    }
    let owners = ["table", "view"];
    if cat.object("temp", &row.tbl_name, &owners).await?.is_some() {
        return Ok(Some("temp".to_owned()));
    }
    for db in cat.databases().await? {
        if !is_temp(&db.name)
            && cat
                .object(&db.name, &row.tbl_name, &owners)
                .await?
                .is_some()
        {
            return Ok(Some(db.name));
        }
    }
    Ok(None)
}

/// `true` when the `temp` trigger `row` fires on a table or view of database `schema`.
async fn temp_trigger_fires_in(cat: &Catalog<'_>, row: &SchemaRow, schema: &str) -> Result<bool> {
    Ok(temp_trigger_target(cat, row)
        .await?
        .is_some_and(|target| target.eq_ignore_ascii_case(schema)))
}

/// The triggers of `owner` with the database each is stored in: its own database's, plus the
/// `TEMP` triggers on it (stored in `temp` whatever database the table is in), sorted by name.
async fn owner_triggers(cat: &Catalog<'_>, owner: &Owner) -> Result<Vec<(SchemaRow, String)>> {
    let mut out = Vec::new();
    if !is_temp(&owner.schema) {
        let own = cat
            .objects_on(&owner.schema, "trigger", owner.name())
            .await?;
        out.extend(own.into_iter().map(|row| (row, owner.schema.clone())));
    }
    for row in cat.objects_on("temp", "trigger", owner.name()).await? {
        if temp_trigger_fires_in(cat, &row, &owner.schema).await? {
            out.push((row, "temp".to_owned()));
        }
    }
    out.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    Ok(out)
}

#[async_trait]
impl Fetcher for TriggerFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let cat = Catalog::new(c, &cancel);
        let owner = Owner::any(&cat, parent).await?;
        let out = owner_triggers(&cat, &owner)
            .await?
            .into_iter()
            .map(|(row, stored_in)| {
                let mut s = ObjectSummary::new(parent.child(ObjectKind::Trigger, row.name));
                if is_temp(&stored_in) {
                    s.extras.insert(props::TEMPORARY, true);
                }
                s
            })
            .collect();
        check(&cancel)?;
        Ok(out)
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        let cat = Catalog::new(c, &cancel);
        let schema = schema_of(obj);
        // The trigger is stored in the ref's database, or it is a `TEMP` trigger on a table of
        // that database (stored in `temp`).
        let (row, stored_in) = match cat.object(schema, &obj.name, &["trigger"]).await? {
            Some(row) => (row, schema.to_owned()),
            None if !is_temp(schema) => {
                let row = cat
                    .object("temp", &obj.name, &["trigger"])
                    .await?
                    .ok_or_else(|| not_found(ObjectKind::Trigger, obj))?;
                if !temp_trigger_fires_in(&cat, &row, schema).await? {
                    return Err(not_found(ObjectKind::Trigger, obj));
                }
                (row, "temp".to_owned())
            }
            None => return Err(not_found(ObjectKind::Trigger, obj)),
        };
        let mut r = obj.clone();
        match obj.parent.as_deref() {
            // A trigger looked up under another table is not this one.
            Some(parent) if !parent.eq_ignore_ascii_case(&row.tbl_name) => {
                return Err(not_found(ObjectKind::Trigger, obj));
            }
            Some(_) => {}
            None => {
                let owner_schema = if is_temp(&stored_in) && is_temp(schema) {
                    temp_trigger_target(&cat, &row)
                        .await?
                        .unwrap_or_else(|| "temp".to_owned())
                } else {
                    schema.to_owned()
                };
                let owner = cat
                    .object(&owner_schema, &row.tbl_name, &["table", "view"])
                    .await
                    .ok()
                    .flatten();
                r.parent = Some(row.tbl_name.clone());
                r.parent_kind = Some(match owner {
                    Some(o) if o.kind == "view" => ObjectKind::View,
                    _ => ObjectKind::Table,
                });
            }
        }
        let def = trigger_def(r, &row, &stored_in)?;
        check(&cancel)?;
        Ok(ObjectDef::Trigger(def))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::Value;

    fn col(name: &str, decl: &str, pk: i64) -> ColumnRow {
        ColumnRow {
            name: name.into(),
            decl: decl.into(),
            not_null: false,
            default: None,
            pk,
            hidden: 0,
        }
    }

    #[test]
    fn rowid_aliases_and_keys() {
        let parsed = parse::parse_create_table(
            "CREATE TABLE t (id INTEGER CONSTRAINT t_pk PRIMARY KEY AUTOINCREMENT, n INT)",
        );
        let (cols, pk) = build_columns(
            &[col("id", "INTEGER", 1), col("n", "INT", 0)],
            &parsed,
            false,
        );
        assert!(cols[0].auto_increment && !cols[0].nullable);
        assert_eq!(cols[0].extras.get(&AUTOINCREMENT), Some(&Value::Bool(true)));
        assert!(!cols[1].auto_increment && cols[1].nullable);
        let pk = pk.unwrap();
        assert_eq!(pk.name.as_deref(), Some("t_pk"));
        assert_eq!(pk.columns, ["id"]);

        // `INT PRIMARY KEY` is not a rowid alias; WITHOUT ROWID keys are NOT NULL.
        let none = ParsedTable::default();
        let (cols, _) = build_columns(&[col("id", "INT", 1)], &none, false);
        assert!(!cols[0].auto_increment && cols[0].nullable);
        let (cols, _) = build_columns(&[col("id", "INTEGER", 1)], &none, true);
        assert!(!cols[0].auto_increment && !cols[0].nullable);
        // Composite keys keep their key order.
        let (_, pk) = build_columns(&[col("a", "INTEGER", 2), col("b", "TEXT", 1)], &none, false);
        assert_eq!(pk.unwrap().columns, ["b", "a"]);

        // A column-level `INTEGER PRIMARY KEY DESC` is not a rowid alias; a table-level
        // `PRIMARY KEY (x DESC)` is.
        let desc = parse::parse_create_table("CREATE TABLE t (id INTEGER PRIMARY KEY DESC)");
        let (cols, _) = build_columns(&[col("id", "INTEGER", 1)], &desc, false);
        assert!(!cols[0].auto_increment && cols[0].nullable);
        let table_desc =
            parse::parse_create_table("CREATE TABLE t (id INTEGER, PRIMARY KEY (id DESC))");
        let (cols, _) = build_columns(&[col("id", "INTEGER", 1)], &table_desc, false);
        assert!(cols[0].auto_increment && !cols[0].nullable);
    }

    #[test]
    fn names_and_actions() {
        assert!(is_system_name("sqlite_sequence"));
        assert!(is_system_name("SQLITE_stat1"));
        assert!(!is_system_name("sqlite"));
        assert!(!is_system_name("é_sqlite_x"));
        assert_eq!(fk_action("SET NULL"), FkAction::SetNull);
        assert_eq!(fk_action("cascade"), FkAction::Cascade);
        assert_eq!(fk_action("NO ACTION"), FkAction::NoAction);
        assert_eq!(schema_of(&ObjectRef::table(None, None, "t")), "main");
        assert_eq!(schema_of(&ObjectRef::database("m2aux")), "m2aux");
    }
}
