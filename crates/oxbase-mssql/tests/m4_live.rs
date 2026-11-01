//! Live tests of the SQL Server DDL generator, comparator, designer capabilities and grid writer
//! against the `mssql` fixture, docs/m4-contracts.md §12 (ddl).
//!
//! All are `#[ignore]`d: the conformance CI job runs them with
//! `cargo test -p oxbase-mssql -- --include-ignored`. The cells come from
//! `oxbase_mssql::dialect::register` (the registry the app uses). Scripts run like the designer
//! runs them: every main step in one transaction.
//!
//! **Re-fetch.** The SQL Server catalog fetchers belong to the fetch stream and are not in this
//! branch yet, so these tests read tables back with their own minimal catalog loader
//! ([`load_table`]: `sys.*` views, bound parameters) and compare with the registered
//! comparator. The integrator replaces `load_table` with the registry's `Table` fetcher
//! (`reg.fetcher(Engine::MsSql, ObjectKind::Table)`), which must pass the same assertions.
//!
//! Scratch tables are named `m3_rt_ddl<kind>_<token>` in schema `m3` (the `[m3]`
//! `roundtrip_prefix`, so the conformance stale cleanup also removes leftovers of killed runs)
//! and are dropped afterwards whatever happened. Grid-writer tests use the `[m3.mssql]` seed
//! table `m3.m3_edit_pk` (with its audit trigger) and only touch rows they insert.

#![allow(
    clippy::many_single_char_names,
    reason = "rows, columns and connections read best with short names in catalog code"
)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arrow_array::cast::AsArray;
use arrow_array::types::Int64Type;
use arrow_array::{Array, RecordBatch};
use arrow_schema::DataType as ArrowType;
use oxbase_dialect::{Comparator, DdlContext, DdlGenerator, DialectRegistry, DmlTarget};
use oxbase_driver::{CancellationToken, Collected, ConnectParams, Connection, collect};
use oxbase_mssql::{MsSqlConnection, MsSqlDriver, extras};
use oxbase_types::{
    CheckDef, ColumnDef, ColumnValue, DataType, Engine, Extras, FkAction, ForeignKeyDef,
    GeneratedColumn, IdentityDef, IndexColumn, IndexDef, KeyKind, ObjectDef, ObjectKind, ObjectRef,
    PrimaryKeyDef, Rename, RowEdit, RowKey, Script, ServerVersion, TableDef, TlsMode, TlsOptions,
    Value, props,
};

type Step<T> = Result<T, String>;

/// One change of the column cell, applied to the column as fetched.
type ColumnChange = fn(&mut ColumnDef);

// --- fixtures -------------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn fixtures() -> toml::Table {
    let path = std::env::var("OXBASE_FIXTURES")
        .ok()
        .filter(|p| !p.is_empty())
        .map_or_else(
            || workspace_root().join("tests/fixtures.toml"),
            |p| {
                let p = PathBuf::from(p);
                if p.is_absolute() {
                    p
                } else {
                    workspace_root().join(p)
                }
            },
        );
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.parse().expect("fixtures.toml parses")
}

fn s(t: &toml::Table, path: &[&str]) -> String {
    let mut table = t;
    for key in &path[..path.len() - 1] {
        table = table
            .get(*key)
            .and_then(toml::Value::as_table)
            .unwrap_or_else(|| panic!("fixtures.toml has no {}", path.join(".")));
    }
    let last = path[path.len() - 1];
    match table.get(last) {
        Some(toml::Value::String(v)) => v.clone(),
        Some(toml::Value::Integer(v)) => v.to_string(),
        _ => panic!("fixtures.toml has no {}", path.join(".")),
    }
}

fn params() -> ConnectParams {
    let f = fixtures();
    ConnectParams::network(Engine::MsSql, s(&f, &["engines", "mssql", "host"]))
        .with_port(s(&f, &["engines", "mssql", "port"]).parse().expect("port"))
        .with_user(s(&f, &["engines", "mssql", "user"]))
        .with_password(s(&f, &["engines", "mssql", "password"]))
        .with_database(s(&f, &["m3", "mssql", "database"]))
        .with_tls(TlsOptions::new(TlsMode::Disable))
}

async fn connect() -> MsSqlConnection {
    MsSqlDriver::new()
        .connect_mssql(&params())
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"))
}

fn registry() -> DialectRegistry {
    let mut reg = DialectRegistry::new();
    oxbase_mssql::dialect::register(&mut reg);
    reg
}

/// A token for scratch names: base36 unix seconds and the process id.
fn token() -> String {
    fn base36(mut n: u64, width: usize) -> String {
        let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let mut out = Vec::new();
        for _ in 0..width {
            out.push(digits[usize::try_from(n % 36).unwrap_or(0)]);
            n /= 36;
        }
        out.reverse();
        String::from_utf8(out).unwrap_or_default()
    }
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!(
        "{}{}",
        base36(secs, 6),
        base36(u64::from(std::process::id()), 4)
    )
}

// --- running SQL ------------------------------------------------------------------------------

async fn exec(c: &dyn Connection, sql: &str, params: &[Value]) -> Step<Collected> {
    let stream = c
        .execute(sql, params, CancellationToken::new())
        .await
        .map_err(|e| format!("{sql}: {e}"))?;
    collect(stream).await.map_err(|e| format!("{sql}: {e}"))
}

/// Runs every step of `script` in one transaction, as the designer does on SQL Server.
async fn run_script(c: &dyn Connection, script: &Script, op: &str) -> Step<()> {
    let tx = c.begin().await.map_err(|e| format!("{op}: begin: {e}"))?;
    for step in script.ordered() {
        let result = match tx.execute(&step.sql, &[], CancellationToken::new()).await {
            Ok(stream) => collect(stream).await.map(|_| ()),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            let _ = tx.rollback().await;
            return Err(format!(
                "{op}: step failed: {e}\n--- step ---\n{}\n--- script ---\n{script}",
                step.sql
            ));
        }
    }
    tx.commit().await.map_err(|e| format!("{op}: commit: {e}"))
}

/// One cell as text (`None` for NULL).
fn cell(batch: &RecordBatch, col: usize, row: usize) -> Option<String> {
    let a = batch.column(col);
    if a.is_null(row) {
        return None;
    }
    Some(match a.data_type() {
        ArrowType::Int64 => a.as_primitive::<Int64Type>().value(row).to_string(),
        ArrowType::Boolean => if a.as_boolean().value(row) { "1" } else { "0" }.to_owned(),
        ArrowType::Utf8 => a.as_string::<i32>().value(row).to_owned(),
        ArrowType::LargeUtf8 => a.as_string::<i64>().value(row).to_owned(),
        ArrowType::Binary => {
            let mut hex = String::new();
            for b in a.as_binary::<i32>().value(row) {
                let _ = write!(hex, "{b:02x}");
            }
            hex
        }
        other => format!("<{other}>"),
    })
}

/// Every row of the first result set, as text.
fn rows(out: &Collected) -> Vec<Vec<Option<String>>> {
    let mut all = Vec::new();
    if let Some(set) = out.result_sets.first() {
        for batch in &set.batches {
            for row in 0..batch.num_rows() {
                all.push(
                    (0..batch.num_columns())
                        .map(|c| cell(batch, c, row))
                        .collect(),
                );
            }
        }
    }
    all
}

/// Rows of every result set (what the edit service counts for `returning` plans).
fn row_count(out: &Collected) -> usize {
    out.result_sets
        .iter()
        .map(oxbase_driver::ResultSet::num_rows)
        .sum()
}

async fn query(c: &dyn Connection, sql: &str, params: &[Value]) -> Step<Vec<Vec<Option<String>>>> {
    Ok(rows(&exec(c, sql, params).await?))
}

fn text(v: Option<&Option<String>>) -> Option<String> {
    v.cloned().flatten()
}

fn int(v: Option<&Option<String>>) -> i64 {
    text(v).and_then(|t| t.parse().ok()).unwrap_or(0)
}

fn yes(v: Option<&Option<String>>) -> bool {
    text(v).as_deref() == Some("1")
}

// --- a minimal catalog loader (until the fetch stream's Table fetcher lands) ------------------

/// The DDL spelling of a column type from `sys.types` and `sys.columns`.
fn native_type(
    name: &str,
    schema: &str,
    user: bool,
    max_length: i64,
    precision: i64,
    scale: i64,
) -> String {
    if user {
        return format!("{schema}.{name}");
    }
    let len = |n: i64| {
        if n < 0 {
            "max".to_owned()
        } else {
            n.to_string()
        }
    };
    match name {
        "varchar" | "char" | "varbinary" | "binary" => format!("{name}({})", len(max_length)),
        "nvarchar" | "nchar" => format!(
            "{name}({})",
            if max_length < 0 {
                "max".to_owned()
            } else {
                (max_length / 2).to_string()
            }
        ),
        "decimal" | "numeric" => format!("{name}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" => format!("{name}({scale})"),
        "timestamp" => "rowversion".to_owned(),
        other => other.to_owned(),
    }
}

fn data_type_of(native: &str) -> DataType {
    let base = native.split('(').next().unwrap_or(native);
    let arg = native
        .split_once('(')
        .and_then(|(_, rest)| rest.trim_end_matches(')').split(',').next())
        .and_then(|a| a.parse::<u32>().ok());
    match base {
        "bit" => DataType::Bool,
        "tinyint" => DataType::Int {
            bits: 8,
            unsigned: true,
        },
        "smallint" => DataType::INT16,
        "int" => DataType::INT32,
        "bigint" => DataType::INT64,
        "decimal" | "numeric" => DataType::Decimal {
            precision: arg,
            scale: None,
        },
        "varchar" | "nvarchar" if arg.is_some() => DataType::VarChar { length: arg },
        "varchar" | "nvarchar" => DataType::Text,
        "varbinary" => DataType::VarBinary { length: arg },
        "date" => DataType::Date,
        "datetime2" => DataType::Timestamp { precision: None },
        "json" => DataType::Json { binary: true },
        "rowversion" => DataType::Binary { length: Some(8) },
        other => DataType::Other {
            name: other.to_owned(),
        },
    }
}

/// Loads `schema.name` from the catalog of the session's database, `None` when it is gone.
async fn load_table(
    c: &dyn Connection,
    db: &str,
    schema: &str,
    name: &str,
) -> Step<Option<TableDef>> {
    let id = query(
        c,
        "SELECT o.object_id FROM sys.objects AS o JOIN sys.schemas AS s ON s.schema_id = \
         o.schema_id WHERE s.name = @P1 AND o.name = @P2 AND o.type = 'U'",
        &[Value::Text(schema.into()), Value::Text(name.into())],
    )
    .await?;
    let Some(id) = id.first().map(|r| int(r.first())) else {
        return Ok(None);
    };
    let oid = [Value::Int(id)];
    let mut t = TableDef::new(ObjectRef::table(Some(db), Some(schema), name));

    let cols = query(
        c,
        "SELECT c.name, t.name, c.max_length, c.precision, c.scale, c.is_nullable, \
         CAST(ic.seed_value AS bigint), CAST(ic.increment_value AS bigint), cc.definition, \
         cc.is_persisted, dc.name, dc.definition, \
         CASE WHEN c.collation_name COLLATE DATABASE_DEFAULT <> CAST(DATABASEPROPERTYEX(DB_NAME(), \
         'Collation') AS nvarchar(128)) COLLATE DATABASE_DEFAULT THEN c.collation_name END, CAST(ep.value AS nvarchar(max)), \
         SCHEMA_NAME(t.schema_id), t.is_user_defined, c.is_identity \
         FROM sys.columns AS c JOIN sys.types AS t ON t.user_type_id = c.user_type_id \
         LEFT JOIN sys.identity_columns AS ic ON ic.object_id = c.object_id AND ic.column_id = \
         c.column_id \
         LEFT JOIN sys.computed_columns AS cc ON cc.object_id = c.object_id AND cc.column_id = \
         c.column_id \
         LEFT JOIN sys.default_constraints AS dc ON dc.parent_object_id = c.object_id AND \
         dc.parent_column_id = c.column_id \
         LEFT JOIN sys.extended_properties AS ep ON ep.class = 1 AND ep.major_id = c.object_id \
         AND ep.minor_id = c.column_id AND ep.name = N'MS_Description' \
         WHERE c.object_id = @P1 ORDER BY c.column_id",
        &oid,
    )
    .await?;
    for r in &cols {
        let native = native_type(
            &text(r.get(1)).unwrap_or_default(),
            &text(r.get(14)).unwrap_or_default(),
            yes(r.get(15)),
            int(r.get(2)),
            int(r.get(3)),
            int(r.get(4)),
        );
        let mut col = ColumnDef::new(text(r.first()).unwrap_or_default(), data_type_of(&native));
        col.native_type.clone_from(&native);
        col.nullable = yes(r.get(5));
        if yes(r.get(16)) {
            col.auto_increment = true;
            col.identity = Some(IdentityDef {
                always: true,
                start: Some(i128::from(int(r.get(6)))),
                increment: Some(i128::from(int(r.get(7)))),
            });
        }
        if let Some(definition) = text(r.get(8)) {
            col.generated = Some(GeneratedColumn {
                expression: definition,
                stored: yes(r.get(9)),
            });
        }
        if let Some(df) = text(r.get(10)) {
            col.extras.insert(extras::DEFAULT_CONSTRAINT, df);
            col.default = text(r.get(11));
        }
        col.collation = text(r.get(12));
        col.comment = text(r.get(13));
        if native == "rowversion" {
            col.extras.insert(extras::ROW_VERSION, true);
        }
        t.columns.push(col);
    }

    let indexes = query(
        c,
        "SELECT i.index_id, i.name, i.is_primary_key, i.is_unique, i.is_unique_constraint, \
         i.type_desc, i.filter_definition, i.fill_factor FROM sys.indexes AS i \
         WHERE i.object_id = @P1 AND i.index_id > 0 ORDER BY i.name",
        &oid,
    )
    .await?;
    let parts = query(
        c,
        "SELECT ic.index_id, c.name, ic.is_descending_key, ic.is_included_column \
         FROM sys.index_columns AS ic JOIN sys.columns AS c ON c.object_id = ic.object_id AND \
         c.column_id = ic.column_id WHERE ic.object_id = @P1 \
         ORDER BY ic.index_id, ic.key_ordinal, ic.index_column_id",
        &oid,
    )
    .await?;
    for r in &indexes {
        let index_id = int(r.first());
        let mut keys = Vec::new();
        let mut include = Vec::new();
        for p in parts.iter().filter(|p| int(p.first()) == index_id) {
            let name = text(p.get(1)).unwrap_or_default();
            if yes(p.get(3)) {
                include.push(serde_json::Value::String(name));
            } else {
                let mut k = IndexColumn::column(name);
                k.descending = yes(p.get(2));
                keys.push(k);
            }
        }
        let kind = text(r.get(5)).unwrap_or_default().to_lowercase();
        let clustered = kind == "clustered";
        let name = text(r.get(1)).unwrap_or_default();
        if yes(r.get(2)) {
            t.primary_key = Some(PrimaryKeyDef {
                name: Some(name),
                columns: keys.into_iter().map(|k| k.name).collect(),
            });
            t.extras.insert(props::CLUSTERED, clustered);
            continue;
        }
        let mut idx = IndexDef {
            name,
            columns: keys,
            unique: yes(r.get(3)),
            is_constraint: yes(r.get(4)),
            predicate: text(r.get(6)),
            extras: Extras::new(),
        };
        idx.extras.insert(props::CLUSTERED, clustered);
        idx.extras.insert(extras::INDEX_KIND, kind);
        if !include.is_empty() {
            idx.extras.insert(
                props::INCLUDE_COLUMNS,
                Value::Json(serde_json::Value::Array(include)),
            );
        }
        let fill = int(r.get(7));
        if fill != 0 {
            idx.extras.insert(props::FILL_FACTOR, Value::Int(fill));
        }
        t.indexes.push(idx);
    }

    let fks = query(
        c,
        "SELECT fk.object_id, fk.name, OBJECT_SCHEMA_NAME(fk.referenced_object_id), \
         OBJECT_NAME(fk.referenced_object_id), fk.delete_referential_action_desc, \
         fk.update_referential_action_desc, fk.is_not_trusted FROM sys.foreign_keys AS fk \
         WHERE fk.parent_object_id = @P1 ORDER BY fk.name",
        &oid,
    )
    .await?;
    let fk_cols = query(
        c,
        "SELECT fkc.constraint_object_id, pc.name, rc.name FROM sys.foreign_key_columns AS fkc \
         JOIN sys.columns AS pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = \
         fkc.parent_column_id JOIN sys.columns AS rc ON rc.object_id = \
         fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
         WHERE fkc.parent_object_id = @P1 ORDER BY fkc.constraint_object_id, \
         fkc.constraint_column_id",
        &oid,
    )
    .await?;
    let action = |desc: Option<String>| match desc.as_deref() {
        Some("CASCADE") => FkAction::Cascade,
        Some("SET_NULL") => FkAction::SetNull,
        Some("SET_DEFAULT") => FkAction::SetDefault,
        _ => FkAction::NoAction,
    };
    for r in &fks {
        let fid = int(r.first());
        let mine: Vec<&Vec<Option<String>>> =
            fk_cols.iter().filter(|p| int(p.first()) == fid).collect();
        let mut fk = ForeignKeyDef {
            name: text(r.get(1)),
            columns: mine
                .iter()
                .map(|p| text(p.get(1)).unwrap_or_default())
                .collect(),
            ref_table: ObjectRef::table(
                Some(db),
                text(r.get(2)).as_deref(),
                text(r.get(3)).unwrap_or_default(),
            ),
            ref_columns: mine
                .iter()
                .map(|p| text(p.get(2)).unwrap_or_default())
                .collect(),
            on_delete: action(text(r.get(4))),
            on_update: action(text(r.get(5))),
            extras: Extras::new(),
        };
        fk.extras.insert(extras::NOT_TRUSTED, yes(r.get(6)));
        t.foreign_keys.push(fk);
    }

    let checks = query(
        c,
        "SELECT name, definition, is_not_trusted FROM sys.check_constraints \
         WHERE parent_object_id = @P1 ORDER BY name",
        &oid,
    )
    .await?;
    for r in &checks {
        let mut check = CheckDef {
            name: text(r.first()),
            expression: text(r.get(1)).unwrap_or_default(),
            extras: Extras::new(),
        };
        check.extras.insert(extras::NOT_TRUSTED, yes(r.get(2)));
        t.checks.push(check);
    }

    let comment = query(
        c,
        "SELECT CAST(value AS nvarchar(max)) FROM sys.extended_properties WHERE class = 1 AND \
         major_id = @P1 AND minor_id = 0 AND name = N'MS_Description'",
        &oid,
    )
    .await?;
    t.comment = comment.first().and_then(|r| text(r.first()));
    Ok(Some(t))
}

// --- the round trip -------------------------------------------------------------------------

struct Rt {
    conn: MsSqlConnection,
    reg: DialectRegistry,
    version: ServerVersion,
    db: String,
    schema: String,
}

impl Rt {
    fn table_ref(&self, name: &str) -> ObjectRef {
        ObjectRef::table(Some(&self.db), Some(&self.schema), name)
    }

    fn cmp(&self) -> Arc<dyn Comparator> {
        self.reg
            .comparator(Engine::MsSql, ObjectKind::Table)
            .expect("table comparator")
    }

    fn ddl(&self) -> Arc<dyn DdlGenerator> {
        self.reg
            .ddl(Engine::MsSql, ObjectKind::Table)
            .expect("table ddl")
    }

    async fn load(&self, name: &str, op: &str) -> Step<TableDef> {
        load_table(&self.conn, &self.db, &self.schema, name)
            .await?
            .ok_or_else(|| format!("{op}: {name} is not in the catalog"))
    }

    /// The table on the server equals `intended` for the comparator.
    async fn same(&self, intended: &TableDef, op: &str) -> Step<TableDef> {
        let fetched = self.load(&intended.obj.name, op).await?;
        let diffs = self.cmp().diff(
            &ObjectDef::Table(intended.clone()),
            &ObjectDef::Table(fetched.clone()),
        );
        if diffs.is_empty() {
            Ok(fetched)
        } else {
            let list: Vec<String> = diffs.iter().map(ToString::to_string).collect();
            Err(format!("{op}: fetched table differs: {}", list.join("; ")))
        }
    }

    async fn create(&self, def: &TableDef, op: &str) -> Step<TableDef> {
        let script = self
            .ddl()
            .create(
                &ObjectDef::Table(def.clone()),
                &DdlContext::new(&self.version),
            )
            .map_err(|e| format!("{op}: {e}"))?;
        run_script(&self.conn, &script, op).await?;
        self.same(def, op).await
    }

    /// Alters `cur` into `new` with the table generator (renames given), checks the result.
    async fn alter(
        &self,
        cur: &TableDef,
        new: &TableDef,
        renames: &[Rename],
        op: &str,
    ) -> Step<TableDef> {
        let ctx = DdlContext::new(&self.version)
            .with_table(cur)
            .with_renames(renames);
        let script = self
            .ddl()
            .alter(
                &ObjectDef::Table(cur.clone()),
                &ObjectDef::Table(new.clone()),
                &ctx,
            )
            .map_err(|e| format!("{op}: {e}"))?;
        if script.is_empty() {
            return Err(format!("{op}: empty script"));
        }
        run_script(&self.conn, &script, op).await?;
        self.same(new, op).await
    }

    /// Changes one child with its kind's generator (`old` / `new` `None` = add / drop).
    async fn child(
        &self,
        table: &TableDef,
        old: Option<&ObjectDef>,
        new: Option<&ObjectDef>,
        renames: &[Rename],
        intended: &TableDef,
        op: &str,
    ) -> Step<TableDef> {
        let kind = new.or(old).map_or(ObjectKind::Table, ObjectDef::kind);
        let ddl = self
            .reg
            .ddl(Engine::MsSql, kind)
            .map_err(|e| e.to_string())?;
        let ctx = DdlContext::new(&self.version)
            .with_table(table)
            .with_renames(renames);
        let script = match (old, new) {
            (None, Some(n)) => ddl.create(n, &ctx),
            (Some(o), Some(n)) => ddl.alter(o, n, &ctx),
            (Some(o), None) => ddl.drop_object(&o.object_ref(), &ctx),
            (None, None) => return Err(format!("{op}: nothing to do")),
        }
        .map_err(|e| format!("{op}: {e}"))?;
        if script.is_empty() {
            return Err(format!("{op}: empty script"));
        }
        run_script(&self.conn, &script, op).await?;
        self.same(intended, op).await
    }

    async fn count(&self, table: &str) -> Step<i64> {
        let r = query(
            &self.conn,
            &format!("SELECT COUNT(*) FROM [{}].[{table}]", self.schema),
            &[],
        )
        .await?;
        Ok(int(r.first().and_then(|row| row.first())))
    }

    async fn drop_if_exists(&self, name: &str) {
        let full = format!("[{}].[{}]", self.schema, name.replace(']', "]]"));
        let _ = exec(
            &self.conn,
            &format!(
                "IF OBJECT_ID(N'{}') IS NOT NULL DROP TABLE {full}",
                full.replace('\'', "''")
            ),
            &[],
        )
        .await;
    }
}

fn col(name: &str, data_type: DataType) -> ColumnDef {
    ColumnDef::new(name, data_type)
}

/// The conformance base table: `id bigint` (PK), `code varchar(40) NOT NULL`, `qty bigint
/// DEFAULT 0`, `"order"` text, neutral types only.
fn base_table(obj: ObjectRef) -> TableDef {
    let mut qty = col("qty", DataType::INT64);
    qty.default = Some("0".into());
    let mut t = TableDef::new(obj);
    t.columns = vec![
        col("id", DataType::INT64).not_null(),
        col("code", DataType::VarChar { length: Some(40) }).not_null(),
        qty,
        col("order", DataType::Text),
    ];
    t.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec!["id".into()],
    });
    t
}

fn with_column(t: &TableDef, c: ColumnDef) -> TableDef {
    let mut out = t.clone();
    match out.columns.iter_mut().find(|x| x.name == c.name) {
        Some(slot) => *slot = c,
        None => out.columns.push(c),
    }
    out
}

async fn round_trips(rt: &Rt, names: &[String]) -> Step<()> {
    let cmp = rt.cmp();
    // Table: create with a comment (quote, backslash, non-ASCII), probe the comparator.
    let mut def = base_table(rt.table_ref(&names[0]));
    def.comment = Some("Round trip: it's a \\ test, caf\u{e9}".into());
    let fetched = rt.create(&def, "create table").await?;
    for (what, change) in [("code 40 -> 41", 0), ("qty default 0 -> 1", 1)] {
        let mut probe = fetched.clone();
        if change == 0 {
            let c = probe
                .columns
                .iter_mut()
                .find(|c| c.name == "code")
                .ok_or("no code")?;
            c.native_type = c.native_type.replace("40", "41");
        } else {
            let c = probe
                .columns
                .iter_mut()
                .find(|c| c.name == "qty")
                .ok_or("no qty")?;
            c.default = Some("1".into());
        }
        if cmp
            .diff(&ObjectDef::Table(fetched.clone()), &ObjectDef::Table(probe))
            .is_empty()
        {
            return Err(format!("the comparator cannot see {what}"));
        }
    }
    let row = [
        Value::Int(1),
        Value::Text("it's a \\ row, \u{e9}".into()),
        Value::Int(5),
        Value::Text("o\"rder \u{fc}".into()),
    ];
    exec(
        &rt.conn,
        &format!(
            "INSERT INTO [m3].[{}] ([id], [code], [qty], [order]) VALUES (@P1, @P2, @P3, @P4)",
            names[0]
        ),
        &row,
    )
    .await?;

    // Rename and change the comment.
    let mut renamed = fetched.clone();
    renamed.obj = rt.table_ref(&names[1]);
    renamed.comment = Some("Changed: \"quoted\" and it's \u{fc}".into());
    let renames = [Rename::new(ObjectKind::Table, &names[0], &names[1])];
    let mut cur = rt
        .alter(&fetched, &renamed, &renames, "rename table")
        .await?;
    if load_table(&rt.conn, &rt.db, &rt.schema, &names[0])
        .await?
        .is_some()
    {
        return Err("rename table: the old name still exists".into());
    }
    let t = cur.obj.clone();

    // Column cell: add, fill, NOT NULL, literal default, comment, wider type, rename, drop.
    let added = col("c_extra", DataType::VarChar { length: Some(40) });
    let intended = with_column(&cur, added.clone());
    cur = rt
        .child(
            &cur,
            None,
            Some(&ObjectDef::Column {
                table: t.clone(),
                column: added,
            }),
            &[],
            &intended,
            "add column",
        )
        .await?;
    exec(
        &rt.conn,
        &format!("UPDATE [m3].[{}] SET [c_extra] = @P1", names[1]),
        &[Value::Text("kept \u{ef}".into())],
    )
    .await?;
    let steps: [(&str, ColumnChange); 4] = [
        ("set column NOT NULL", |c| c.nullable = false),
        ("set column default", |c| {
            c.default = Some("N'it''s \u{fc}'".into());
        }),
        ("set column comment", |c| {
            c.comment = Some("Note: it's a \\ column, na\u{ef}ve".into());
        }),
        ("change column type", |c| {
            c.data_type = DataType::VarChar { length: Some(80) };
            c.native_type = String::new();
        }),
    ];
    for (op, change) in steps {
        let old = cur.column("c_extra").ok_or("no c_extra")?.clone();
        let mut new = old.clone();
        change(&mut new);
        let intended = with_column(&cur, new.clone());
        cur = rt
            .child(
                &cur,
                Some(&ObjectDef::Column {
                    table: t.clone(),
                    column: old,
                }),
                Some(&ObjectDef::Column {
                    table: t.clone(),
                    column: new,
                }),
                &[],
                &intended,
                op,
            )
            .await?;
    }
    let reserved = "sel\"ect_\u{fc}";
    let old = cur.column("c_extra").ok_or("no c_extra")?.clone();
    let mut new = old.clone();
    reserved.clone_into(&mut new.name);
    let mut intended = cur.clone();
    if let Some(c) = intended.columns.iter_mut().find(|c| c.name == "c_extra") {
        reserved.clone_into(&mut c.name);
    }
    cur = rt
        .child(
            &cur,
            Some(&ObjectDef::Column {
                table: t.clone(),
                column: old,
            }),
            Some(&ObjectDef::Column {
                table: t.clone(),
                column: new.clone(),
            }),
            &[Rename::new(ObjectKind::Column, "c_extra", reserved)],
            &intended,
            "rename column",
        )
        .await?;
    let mut intended = cur.clone();
    intended.columns.retain(|c| c.name != reserved);
    cur = rt
        .child(
            &cur,
            Some(&ObjectDef::Column {
                table: t.clone(),
                column: new,
            }),
            None,
            &[],
            &intended,
            "drop column",
        )
        .await?;

    // Index cell.
    let ix_name = format!("{}_ix", names[1]);
    let ix = IndexDef {
        name: ix_name.clone(),
        columns: vec![IndexColumn::column("code")],
        unique: false,
        is_constraint: false,
        predicate: None,
        extras: Extras::new(),
    };
    let mut intended = cur.clone();
    intended.indexes.push(ix.clone());
    cur = rt
        .child(
            &cur,
            None,
            Some(&ObjectDef::Index {
                table: t.clone(),
                index: ix,
            }),
            &[],
            &intended,
            "add index",
        )
        .await?;
    let old = cur
        .indexes
        .iter()
        .find(|i| i.name == ix_name)
        .ok_or("no index")?
        .clone();
    let mut new = old.clone();
    new.columns = vec![IndexColumn::column("code"), IndexColumn::column("qty")];
    new.unique = true;
    let mut intended = cur.clone();
    intended.indexes = vec![new.clone()];
    cur = rt
        .child(
            &cur,
            Some(&ObjectDef::Index {
                table: t.clone(),
                index: old,
            }),
            Some(&ObjectDef::Index {
                table: t.clone(),
                index: new.clone(),
            }),
            &[],
            &intended,
            "change index",
        )
        .await?;
    let mut intended = cur.clone();
    intended.indexes.clear();
    cur = rt
        .child(
            &cur,
            Some(&ObjectDef::Index {
                table: t.clone(),
                index: new,
            }),
            None,
            &[],
            &intended,
            "drop index",
        )
        .await?;

    // Check cell, with an enforcement probe.
    let ck = CheckDef {
        name: Some(format!("{}_ck", names[1])),
        expression: "qty >= 0".into(),
        extras: Extras::new(),
    };
    let mut intended = cur.clone();
    intended.checks.push(ck.clone());
    cur = rt
        .child(
            &cur,
            None,
            Some(&ObjectDef::Check {
                table: t.clone(),
                check: ck.clone(),
            }),
            &[],
            &intended,
            "add check",
        )
        .await?;
    let violating = exec(
        &rt.conn,
        &format!(
            "INSERT INTO [m3].[{}] ([id], [code], [qty]) VALUES (@P1, @P2, @P3)",
            names[1]
        ),
        &[Value::Int(3), Value::Text("probe".into()), Value::Int(-1)],
    )
    .await;
    if violating.is_ok() {
        return Err("checksEnforced: a row violating the check was accepted".into());
    }
    let old = cur.checks.first().ok_or("no check")?.clone();
    let mut changed = old.clone();
    changed.expression = "qty >= 1".into();
    let mut intended = cur.clone();
    intended.checks = vec![changed.clone()];
    cur = rt
        .child(
            &cur,
            Some(&ObjectDef::Check {
                table: t.clone(),
                check: old,
            }),
            Some(&ObjectDef::Check {
                table: t.clone(),
                check: changed.clone(),
            }),
            &[],
            &intended,
            "change check",
        )
        .await?;
    let mut intended = cur.clone();
    intended.checks.clear();
    cur = rt
        .child(
            &cur,
            Some(&ObjectDef::Check {
                table: t.clone(),
                check: changed,
            }),
            None,
            &[],
            &intended,
            "drop check",
        )
        .await?;

    // Foreign key cell: a parent table, a column and an index on the child.
    let mut parent = TableDef::new(rt.table_ref(&names[2]));
    parent.columns = vec![col("id", DataType::INT64).not_null()];
    parent.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec!["id".into()],
    });
    let parent = rt.create(&parent, "create parent table").await?;
    exec(
        &rt.conn,
        &format!("INSERT INTO [m3].[{}] ([id]) VALUES (@P1)", names[2]),
        &[Value::Int(1)],
    )
    .await?;
    let mut with_ref = with_column(&cur, col("ref_id", DataType::INT64));
    with_ref.indexes.push(IndexDef {
        name: format!("{}_ref_ix", names[1]),
        columns: vec![IndexColumn::column("ref_id")],
        unique: false,
        is_constraint: false,
        predicate: None,
        extras: Extras::new(),
    });
    cur = rt
        .alter(&cur, &with_ref, &[], "add ref_id and its index")
        .await?;
    let fk = ForeignKeyDef {
        name: Some(format!("{}_fk", names[1])),
        columns: vec!["ref_id".into()],
        ref_table: parent.obj.clone(),
        ref_columns: vec!["id".into()],
        on_delete: FkAction::Cascade,
        on_update: FkAction::NoAction,
        extras: Extras::new(),
    };
    let mut intended = cur.clone();
    intended.foreign_keys.push(fk.clone());
    cur = rt
        .child(
            &cur,
            None,
            Some(&ObjectDef::ForeignKey {
                table: t.clone(),
                foreign_key: fk,
            }),
            &[],
            &intended,
            "add foreign key",
        )
        .await?;
    let old = cur.foreign_keys.first().ok_or("no foreign key")?.clone();
    let mut other = old.clone();
    other.ref_table.name.push_str("_other");
    if cmp
        .diff(
            &ObjectDef::Table(with_fk(&cur, &old)),
            &ObjectDef::Table(with_fk(&cur, &other)),
        )
        .is_empty()
    {
        return Err("the comparator cannot see another referenced table".into());
    }
    let mut set_null = old.clone();
    set_null.on_delete = FkAction::SetNull;
    let mut intended = cur.clone();
    intended.foreign_keys = vec![set_null.clone()];
    cur = rt
        .child(
            &cur,
            Some(&ObjectDef::ForeignKey {
                table: t.clone(),
                foreign_key: old,
            }),
            Some(&ObjectDef::ForeignKey {
                table: t.clone(),
                foreign_key: set_null.clone(),
            }),
            &[],
            &intended,
            "change foreign key",
        )
        .await?;
    let mut intended = cur.clone();
    intended.foreign_keys.clear();
    cur = rt
        .child(
            &cur,
            Some(&ObjectDef::ForeignKey {
                table: t.clone(),
                foreign_key: set_null,
            }),
            None,
            &[],
            &intended,
            "drop foreign key",
        )
        .await?;

    // The row is still there with its values.
    let kept = query(
        &rt.conn,
        &format!(
            "SELECT COUNT(*) FROM [m3].[{}] WHERE [id] = @P1 AND [code] = @P2 AND [qty] = @P3 \
             AND [order] = @P4",
            names[1]
        ),
        &row,
    )
    .await?;
    if int(kept.first().and_then(|r| r.first())) != 1 || rt.count(&names[1]).await? != 1 {
        return Err("the row inserted at the start was changed or lost".into());
    }

    // Drop.
    let script = rt
        .ddl()
        .drop_object(&cur.obj, &DdlContext::new(&rt.version).with_table(&cur))
        .map_err(|e| e.to_string())?;
    run_script(&rt.conn, &script, "drop table").await?;
    if load_table(&rt.conn, &rt.db, &rt.schema, &names[1])
        .await?
        .is_some()
    {
        return Err("drop table: the table still exists".into());
    }
    Ok(())
}

fn with_fk(t: &TableDef, fk: &ForeignKeyDef) -> TableDef {
    let mut out = t.clone();
    out.foreign_keys = vec![fk.clone()];
    out
}

#[tokio::test]
#[ignore = "needs the mssql fixture (scripts/fixtures-up.sh mssql)"]
async fn designer_kinds_round_trip_on_sql_server() {
    let conn = connect().await;
    let f = fixtures();
    let rt = Rt {
        version: conn.server_version().clone(),
        conn,
        reg: registry(),
        db: s(&f, &["m3", "mssql", "database"]),
        schema: s(&f, &["m3", "mssql", "schema"]),
    };
    let tok = token();
    let names: Vec<String> = ["a", "b", "ref"]
        .iter()
        .map(|k| format!("m3_rt_ddl{k}_{tok}"))
        .collect();
    let result = round_trips(&rt, &names).await;
    for name in names.iter().rev() {
        rt.drop_if_exists(name).await;
    }
    if let Err(e) = result {
        panic!("{e}");
    }
    // Every scratch table is gone.
    for name in &names {
        let gone = load_table(&rt.conn, &rt.db, &rt.schema, name)
            .await
            .unwrap();
        assert_eq!(gone, None, "{name} is left over");
    }
}

/// A table with what blocks `ALTER COLUMN`: a named default, a filtered unique index with an
/// included column, a clustered index, a check and a computed column on the altered column,
/// an identity key and a nonclustered primary key.
async fn blockers(rt: &Rt, name: &str) -> Step<()> {
    let mut t = TableDef::new(rt.table_ref(name));
    let mut id = ColumnDef::new("id", DataType::INT32).not_null();
    id.native_type = "int".into();
    id.identity = Some(IdentityDef {
        always: true,
        start: Some(5),
        increment: Some(2),
    });
    let mut code = ColumnDef::new("code", DataType::VarChar { length: Some(20) }).not_null();
    code.native_type = "varchar(20)".into();
    code.default = Some("'x'".into());
    code.comment = Some("the code".into());
    let mut qty = ColumnDef::new("qty", DataType::INT32);
    qty.native_type = "int".into();
    let mut up = ColumnDef::new("up", DataType::VarChar { length: Some(20) });
    up.generated = Some(GeneratedColumn {
        expression: "upper(code)".into(),
        stored: false,
    });
    t.columns = vec![id, code, qty, up];
    t.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec!["id".into()],
    });
    t.extras.insert(props::CLUSTERED, false);
    let mut uq = IndexDef {
        name: format!("{name}_code_uq"),
        columns: vec![IndexColumn::column("code")],
        unique: true,
        is_constraint: false,
        predicate: Some("code IS NOT NULL".into()),
        extras: Extras::new(),
    };
    uq.extras.insert(
        props::INCLUDE_COLUMNS,
        Value::Json(serde_json::json!(["qty"])),
    );
    let mut cl = IndexDef {
        name: format!("{name}_qty_cl"),
        columns: vec![IndexColumn::column("qty")],
        unique: false,
        is_constraint: false,
        predicate: None,
        extras: Extras::new(),
    };
    cl.columns[0].descending = true;
    cl.extras.insert(props::CLUSTERED, true);
    t.indexes = vec![uq, cl];
    t.checks.push(CheckDef {
        name: Some(format!("{name}_ck")),
        expression: "len(code) > 0".into(),
        extras: Extras::new(),
    });
    let cur = rt.create(&t, "create blockers table").await?;
    let id = cur.column("id").and_then(|c| c.identity.clone());
    if id.as_ref().and_then(|i| i.start) != Some(5) || id.and_then(|i| i.increment) != Some(2) {
        return Err("IDENTITY(5, 2) was not kept".into());
    }
    exec(
        &rt.conn,
        &format!("INSERT INTO [m3].[{name}] ([code], [qty]) VALUES (@P1, @P2)"),
        &[Value::Text("abc".into()), Value::Int(7)],
    )
    .await?;

    // varchar(20) -> nvarchar(30): every blocker is dropped and recreated.
    let mut new = cur.clone();
    if let Some(c) = new.columns.iter_mut().find(|c| c.name == "code") {
        c.native_type = "nvarchar(30)".into();
    }
    let ctx = DdlContext::new(&rt.version).with_table(&cur);
    let script = rt
        .ddl()
        .alter(
            &ObjectDef::Table(cur.clone()),
            &ObjectDef::Table(new.clone()),
            &ctx,
        )
        .map_err(|e| e.to_string())?;
    run_script(&rt.conn, &script, "alter column with blockers").await?;
    // The computed column moved to the end (it already is the last one here).
    let after = rt.same(&new, "alter column with blockers").await?;
    let up = query(
        &rt.conn,
        &format!("SELECT [up], [id] FROM [m3].[{name}]"),
        &[],
    )
    .await?;
    if text(up.first().and_then(|r| r.first())).as_deref() != Some("ABC")
        || int(up.first().and_then(|r| r.get(1))) != 5
    {
        return Err(format!("the row changed: {up:?}"));
    }
    // Growing the type again keeps the default and the indexes: one statement.
    let mut wider = after.clone();
    if let Some(c) = wider.columns.iter_mut().find(|c| c.name == "code") {
        c.native_type = "nvarchar(40)".into();
    }
    let ctx = DdlContext::new(&rt.version).with_table(&after);
    let script = rt
        .ddl()
        .alter(
            &ObjectDef::Table(after.clone()),
            &ObjectDef::Table(wider.clone()),
            &ctx,
        )
        .map_err(|e| e.to_string())?;
    // The computed column still references the column, so it goes and comes back.
    run_script(&rt.conn, &script, "widen column").await?;
    rt.same(&wider, "widen column").await?;
    // IDENTITY changes are refused.
    let mut no_identity = wider.clone();
    if let Some(c) = no_identity.columns.iter_mut().find(|c| c.name == "id") {
        c.identity = None;
        c.auto_increment = false;
    }
    let made = rt.ddl().alter(
        &ObjectDef::Table(wider.clone()),
        &ObjectDef::Table(no_identity),
        &DdlContext::new(&rt.version).with_table(&wider),
    );
    if !matches!(made, Err(oxbase_types::Error::Unsupported { .. })) {
        return Err(format!(
            "removing IDENTITY: expected Unsupported, got {made:?}"
        ));
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the mssql fixture (scripts/fixtures-up.sh mssql)"]
async fn alter_column_drops_and_recreates_its_blockers() {
    let conn = connect().await;
    let f = fixtures();
    let rt = Rt {
        version: conn.server_version().clone(),
        conn,
        reg: registry(),
        db: s(&f, &["m3", "mssql", "database"]),
        schema: s(&f, &["m3", "mssql", "schema"]),
    };
    let name = format!("m3_rt_ddlblk_{}", token());
    let result = blockers(&rt, &name).await;
    rt.drop_if_exists(&name).await;
    if let Err(e) = result {
        panic!("{e}");
    }
}

// --- grid writer --------------------------------------------------------------------------

async fn run_plan(
    c: &dyn Connection,
    reg: &DialectRegistry,
    edit: &RowEdit,
    table: &TableDef,
    key: &RowKey,
    read_back: &[String],
    v: &ServerVersion,
) -> Step<Collected> {
    let plan = reg
        .dml_writer(Engine::MsSql)
        .map_err(|e| e.to_string())?
        .plan(
            edit,
            &DmlTarget {
                table,
                key,
                version: v,
                read_back,
            },
        )
        .map_err(|e| e.to_string())?;
    if !plan.returning || plan.read_back.is_some() {
        return Err("SQL Server plans return their rows themselves".into());
    }
    exec(c, &plan.write.sql, &plan.write.params).await
}

/// A decimal's text without trailing fractional zeros (`21.00` → `21`).
fn decimal_text(text: &str) -> String {
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text.to_owned()
    }
}

/// The value of `column` in the first row the plan returned.
fn returned(out: &Collected, column: &str) -> Option<String> {
    let set = out.result_sets.first()?;
    let idx = set.schema.index_of(column).ok()?;
    let batch = set.batches.iter().find(|b| b.num_rows() > 0)?;
    cell(batch, idx, 0)
}

async fn grid_writes(c: &dyn Connection, other: &dyn Connection, tok: &str) -> Step<()> {
    let f = fixtures();
    let db = s(&f, &["m3", "mssql", "database"]);
    let schema = s(&f, &["m3", "mssql", "schema"]);
    let table_name = s(&f, &["m3", "edit_pk_table"]);
    let reg = registry();
    let v = c.server_version().clone();
    let table = load_table(c, &db, &schema, &table_name)
        .await?
        .ok_or("the m3.m3_edit_pk seed table is missing (tests/docker/init/mssql/05-m3.sql)")?;
    let key = RowKey::new(table.obj.clone(), KeyKind::Primary, vec!["id".into()]);
    let read_back: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();

    // Insert: exactly one row comes back, with the generated id and the computed total, although
    // the audit trigger inserts rows of its own.
    let insert = RowEdit::Insert {
        values: vec![
            ColumnValue::new("code", format!("t{tok}-1")),
            ColumnValue::new("name", "it's \u{fc}"),
            ColumnValue::new("amount", Value::Decimal("10.50".into())),
            ColumnValue::new("data", Value::Null),
        ],
    };
    let out = run_plan(c, &reg, &insert, &table, &key, &read_back, &v).await?;
    if row_count(&out) != 1 {
        return Err(format!("insert returned {} rows", row_count(&out)));
    }
    let id: i64 = returned(&out, "id")
        .and_then(|t| t.parse().ok())
        .ok_or("insert: no generated id")?;
    let total = returned(&out, "total").ok_or("insert: no total")?;
    if decimal_text(&total) != "21" {
        return Err(format!("insert: total is {total}, expected 21"));
    }
    let audit = query(
        c,
        "SELECT COUNT(*) FROM m3.m3_edit_audit WHERE edit_id = @P1",
        &[Value::Int(id)],
    )
    .await?;
    if int(audit.first().and_then(|r| r.first())) < 1 {
        return Err("the audit trigger did not run".into());
    }

    // Update: one row, read back after the trigger.
    let update = RowEdit::Update {
        key: vec![Value::Int(id)],
        changes: vec![
            ColumnValue::new("amount", Value::Decimal("1.25".into())),
            ColumnValue::new("name", Value::Null),
        ],
    };
    let out = run_plan(c, &reg, &update, &table, &key, &read_back, &v).await?;
    if row_count(&out) != 1 {
        return Err(format!("update returned {} rows", row_count(&out)));
    }
    if returned(&out, "name").is_some() {
        return Err("update: name is not NULL".into());
    }
    if returned(&out, "total").as_deref().map(decimal_text) != Some("2.5".to_owned()) {
        return Err("update: total was not recomputed".into());
    }

    // Writing a computed or identity column is refused.
    let bad = RowEdit::Update {
        key: vec![Value::Int(id)],
        changes: vec![ColumnValue::new("total", Value::Int(1))],
    };
    if run_plan(c, &reg, &bad, &table, &key, &read_back, &v)
        .await
        .is_ok()
    {
        return Err("writing the computed column was not refused".into());
    }

    // Conflict: another session deletes the row; the update finds nothing.
    exec(
        other,
        "DELETE FROM m3.m3_edit_pk WHERE id = @P1",
        &[Value::Int(id)],
    )
    .await?;
    let out = run_plan(c, &reg, &update, &table, &key, &read_back, &v).await?;
    if row_count(&out) != 0 {
        return Err(format!(
            "conflict: update returned {} rows",
            row_count(&out)
        ));
    }

    // Delete: one row.
    let out = run_plan(
        c,
        &reg,
        &RowEdit::Insert {
            values: vec![ColumnValue::new("code", format!("t{tok}-2"))],
        },
        &table,
        &key,
        &[],
        &v,
    )
    .await?;
    let id2: i64 = returned(&out, "id")
        .and_then(|t| t.parse().ok())
        .ok_or("second insert: no id")?;
    let out = run_plan(
        c,
        &reg,
        &RowEdit::Delete {
            key: vec![Value::Int(id2)],
        },
        &table,
        &key,
        &[],
        &v,
    )
    .await?;
    if row_count(&out) != 1 {
        return Err(format!("delete returned {} rows", row_count(&out)));
    }
    let out = run_plan(
        c,
        &reg,
        &RowEdit::Delete {
            key: vec![Value::Int(id2)],
        },
        &table,
        &key,
        &[],
        &v,
    )
    .await?;
    if row_count(&out) != 0 {
        return Err("deleting a deleted row did not count 0".into());
    }

    // A row of defaults (no parameters: a plain batch) on the audit table.
    let audit_table = load_table(c, &db, &schema, &s(&f, &["m4", "edit_audit_table"]))
        .await?
        .ok_or("the m3.m3_edit_audit table is missing")?;
    let audit_key = RowKey::new(audit_table.obj.clone(), KeyKind::Primary, vec!["id".into()]);
    let out = run_plan(
        c,
        &reg,
        &RowEdit::Insert { values: vec![] },
        &audit_table,
        &audit_key,
        &["id".into(), "at".into()],
        &v,
    )
    .await?;
    if row_count(&out) != 1 || returned(&out, "at").is_none() {
        return Err("a row of defaults was not read back".into());
    }
    let audit_id: i64 = returned(&out, "id")
        .and_then(|t| t.parse().ok())
        .unwrap_or(0);
    exec(
        c,
        "DELETE FROM m3.m3_edit_audit WHERE id = @P1",
        &[Value::Int(audit_id)],
    )
    .await?;
    // NOCOUNT is back on for the session.
    let nocount = query(c, "SELECT CAST(@@OPTIONS & 512 AS int)", &[]).await?;
    if int(nocount.first().and_then(|r| r.first())) != 0 {
        return Err("SET NOCOUNT was left ON".into());
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs the mssql fixture (scripts/fixtures-up.sh mssql)"]
async fn grid_writer_counts_exactly_one_row_per_edit() {
    let c = connect().await;
    let other = connect().await;
    let tok = token();
    let result = grid_writes(&c, &other, &tok).await;
    let _ = exec(
        &c,
        "DELETE FROM m3.m3_edit_pk WHERE code LIKE @P1",
        &[Value::Text(format!("t{tok}-%"))],
    )
    .await;
    if let Err(e) = result {
        panic!("{e}");
    }
}
