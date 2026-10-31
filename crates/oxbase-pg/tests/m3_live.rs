//! Live tests of the PostgreSQL M3 cells against the `postgres` fixture: DDL scripts applied to
//! the server and fetched back with the M2 fetchers (the round-trip invariant: the fetched
//! definition compares equal to the one the script was generated from), and grid writes planned
//! by the `DmlWriter` against the `[m3]` seed tables (`tests/docker/init/postgres/05-m3.sql`).
//!
//! Scratch tables are named `<roundtrip_prefix><kind>_<token>` (`tests/fixtures.toml` `[m3]`),
//! dropped by a guard that also runs when a test fails, and leftovers older than
//! `stale_after_secs` are dropped first.
//!
//! All are `#[ignore]`d: run them with
//! `cargo test -p oxbase-pg --test m3_live -- --include-ignored --test-threads 1`.

#![allow(
    clippy::many_single_char_names,
    reason = "the fixture, connection and version are used everywhere as f, c and v"
)]

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use arrow_array::{Array, Int32Array, StringArray};
use futures::FutureExt;
use oxbase_dialect::{DdlContext, DialectRegistry, DmlTarget, PgQuoting, Quoting};
use oxbase_driver::{
    CancellationToken, Collected, ConnectParams, Connection, Driver, ResultSet, TlsMode,
    TlsOptions, collect,
};
use oxbase_pg::PgDriver;
use oxbase_pg::dialect::NOT_VALID;
use oxbase_types::{
    CheckDef, ColumnDef, ColumnValue, DataType, Engine, Error, Extras, FkAction, ForeignKeyDef,
    GeneratedColumn, IdentityDef, IndexColumn, IndexDef, KeyKind, ObjectDef, ObjectKind, ObjectRef,
    PrimaryKeyDef, Rename, RowEdit, RowKey, Script, ServerVersion, StepPhase, TableDef, Value,
    props,
};

struct Fixture {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
    schema: String,
    prefix: String,
    stale_after_secs: u64,
    edit_pk: String,
    edit_uq: String,
}

fn fixture() -> Fixture {
    let path = PathBuf::from(std::env::var("OXBASE_FIXTURES").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures.toml").into()
    }));
    let text = std::fs::read_to_string(&path).expect("read tests/fixtures.toml");
    let doc: toml::Table = text.parse().expect("parse tests/fixtures.toml");
    let pg = doc["engines"]["postgres"]
        .as_table()
        .expect("[engines.postgres]");
    let m3 = doc["m3"].as_table().expect("[m3]");
    let m3pg = m3["postgres"].as_table().expect("[m3.postgres]");
    let s = |t: &toml::Table, k: &str| t[k].as_str().expect(k).to_owned();
    Fixture {
        host: s(pg, "host"),
        port: u16::try_from(pg["port"].as_integer().expect("port")).expect("port"),
        user: s(pg, "user"),
        password: s(pg, "password"),
        database: s(m3pg, "database"),
        schema: s(m3pg, "schema"),
        prefix: s(m3, "roundtrip_prefix"),
        stale_after_secs: u64::try_from(m3["stale_after_secs"].as_integer().expect("stale"))
            .expect("stale"),
        edit_pk: s(m3, "edit_pk_table"),
        edit_uq: s(m3, "edit_unique_table"),
    }
}

impl Fixture {
    fn params(&self) -> ConnectParams {
        ConnectParams::network(Engine::Postgres, &self.host)
            .with_port(self.port)
            .with_user(&self.user)
            .with_password(self.password.as_str())
            .with_database(&self.database)
            .with_tls(TlsOptions::new(TlsMode::Disable))
    }

    fn table_ref(&self, name: &str) -> ObjectRef {
        ObjectRef::database(&self.database)
            .child(ObjectKind::Schema, &self.schema)
            .child(ObjectKind::Table, name)
    }

    fn scratch(&self, kind: &str, token: &str) -> String {
        format!("{}{kind}_{token}", self.prefix)
    }
}

const BASE36: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

fn base36(mut n: u64, width: usize) -> String {
    let mut out = vec![b'0'; width];
    for slot in out.iter_mut().rev() {
        *slot = BASE36[usize::try_from(n % 36).expect("digit")];
        n /= 36;
    }
    String::from_utf8(out).expect("ascii")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

/// 10 lowercase base36 characters: 6 for the unix seconds, 4 for the process id.
fn token() -> String {
    format!(
        "{}{}",
        base36(now_secs(), 6),
        base36(u64::from(std::process::id()) % 36u64.pow(4), 4)
    )
}

/// The unix seconds of a scratch name's token, if it has one.
fn token_secs(name: &str, prefix: &str) -> Option<u64> {
    let rest = name.strip_prefix(prefix)?;
    rest.split('_')
        .find(|part| part.len() == 10 && part.bytes().all(|b| BASE36.contains(&b)))
        .and_then(|part| u64::from_str_radix(&part[..6], 36).ok())
}

async fn connect(f: &Fixture) -> Box<dyn Connection> {
    match PgDriver.connect(&f.params()).await {
        Ok(c) => c,
        Err(e) => panic!("connect failed: {e}"),
    }
}

async fn try_run(c: &dyn Connection, sql: &str, params: &[Value]) -> Result<Collected, Error> {
    collect(c.execute(sql, params, CancellationToken::new()).await?).await
}

async fn run(c: &dyn Connection, sql: &str, params: &[Value]) -> Collected {
    try_run(c, sql, params)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Every column of every row of the first result set, as text (the query casts to text).
fn texts(out: &Collected) -> Vec<Vec<Option<String>>> {
    let Some(set) = out.result_sets.first() else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for batch in &set.batches {
        for row in 0..batch.num_rows() {
            let mut cells = Vec::new();
            for col in batch.columns() {
                let strings = col
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .expect("a text column (cast it in the query)");
                cells.push((!strings.is_null(row)).then(|| strings.value(row).to_owned()));
            }
            rows.push(cells);
        }
    }
    rows
}

fn returned_rows(out: &Collected) -> usize {
    out.result_sets.first().map_or(0, ResultSet::num_rows)
}

fn registry() -> DialectRegistry {
    let mut reg = DialectRegistry::new();
    oxbase_pg::dialect::register(&mut reg);
    reg
}

async fn try_load(c: &dyn Connection, obj: &ObjectRef) -> Result<ObjectDef, Error> {
    registry()
        .fetcher(Engine::Postgres, obj.kind)
        .expect("fetcher")
        .load(c, obj, CancellationToken::new())
        .await
}

async fn load(c: &dyn Connection, obj: &ObjectRef) -> ObjectDef {
    try_load(c, obj)
        .await
        .unwrap_or_else(|e| panic!("load {obj}: {e}"))
}

async fn load_table(c: &dyn Connection, obj: &ObjectRef) -> TableDef {
    match load(c, obj).await {
        ObjectDef::Table(t) => t,
        other => panic!("not a table: {other:?}"),
    }
}

/// Runs a PostgreSQL script the way the designer applies it: all main steps in one
/// transaction (PostgreSQL scripts have no prepare or finally steps).
async fn apply(c: &dyn Connection, script: &Script) {
    assert!(script.is_atomic(), "not atomic: {script}");
    assert!(
        script.steps.iter().all(|s| s.phase == StepPhase::Main),
        "{script}"
    );
    let tx = c.begin().await.expect("begin");
    for step in script.ordered() {
        let stream = tx
            .execute(&step.sql, &[], CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{}: {e}\n--- script ---\n{script}", step.sql));
        collect(stream)
            .await
            .unwrap_or_else(|e| panic!("{}: {e}\n--- script ---\n{script}", step.sql));
    }
    tx.commit().await.expect("commit");
}

/// The round-trip check: `fetched` compares equal to `source` with the registered comparator.
fn assert_same(source: &ObjectDef, fetched: &ObjectDef, script: &Script) {
    let diffs = registry()
        .comparator(Engine::Postgres, source.kind())
        .expect("comparator")
        .diff(source, fetched);
    let text: Vec<String> = diffs.iter().map(ToString::to_string).collect();
    assert_eq!(
        text,
        Vec::<String>::new(),
        "fetched {} differs from its source\n--- script ---\n{script}\n--- fetched ---\n{fetched:#?}",
        source.kind()
    );
}

fn ddl_create(d: &ObjectDef, v: &ServerVersion, table: Option<&TableDef>) -> Script {
    let mut ctx = DdlContext::new(v);
    if let Some(t) = table {
        ctx = ctx.with_table(t);
    }
    registry()
        .ddl(Engine::Postgres, d.kind())
        .expect("ddl")
        .create(d, &ctx)
        .unwrap_or_else(|e| panic!("create {}: {e}", d.kind()))
}

fn ddl_alter(
    old: &ObjectDef,
    new: &ObjectDef,
    v: &ServerVersion,
    table: Option<&TableDef>,
    renames: &[Rename],
) -> Script {
    let mut ctx = DdlContext::new(v).with_renames(renames);
    if let Some(t) = table {
        ctx = ctx.with_table(t);
    }
    registry()
        .ddl(Engine::Postgres, new.kind())
        .expect("ddl")
        .alter(old, new, &ctx)
        .unwrap_or_else(|e| panic!("alter {}: {e}", new.kind()))
}

fn ddl_drop(obj: &ObjectRef, v: &ServerVersion, table: Option<&TableDef>) -> Script {
    let mut ctx = DdlContext::new(v);
    if let Some(t) = table {
        ctx = ctx.with_table(t);
    }
    registry()
        .ddl(Engine::Postgres, obj.kind)
        .expect("ddl")
        .drop_object(obj, &ctx)
        .unwrap_or_else(|e| panic!("drop {obj}: {e}"))
}

async fn assert_gone(c: &dyn Connection, obj: &ObjectRef) {
    match try_load(c, obj).await {
        Err(Error::NotFound { .. }) => {}
        other => panic!("{obj} should be gone: {other:?}"),
    }
}

async fn drop_tables(c: &dyn Connection, schema: &str, tables: &[String]) {
    for t in tables {
        let name = PgQuoting.qualify(&ObjectRef::table(None, Some(schema), t.as_str()));
        if let Err(e) = try_run(c, &format!("DROP TABLE IF EXISTS {name} CASCADE"), &[]).await {
            eprintln!("cleanup of {name} failed: {e}");
        }
    }
}

/// Drops scratch tables of earlier runs whose token is older than `stale_after_secs`.
async fn drop_stale(c: &dyn Connection, f: &Fixture) {
    let out = run(
        c,
        "SELECT tablename::text FROM pg_catalog.pg_tables \
         WHERE schemaname = $1 AND starts_with(tablename, $2)",
        &[Value::Text(f.schema.clone()), Value::Text(f.prefix.clone())],
    )
    .await;
    let now = now_secs();
    let stale: Vec<String> = texts(&out)
        .into_iter()
        .filter_map(|row| row.into_iter().next().flatten())
        .filter(|name| {
            token_secs(name, &f.prefix)
                .is_some_and(|secs| secs <= now && now - secs > f.stale_after_secs)
        })
        .collect();
    drop_tables(c, &f.schema, &stale).await;
}

/// Runs `body`, then drops `tables` whether it passed or panicked.
async fn guarded(
    c: &dyn Connection,
    f: &Fixture,
    tables: &[String],
    body: impl Future<Output = ()>,
) {
    drop_stale(c, f).await;
    drop_tables(c, &f.schema, tables).await;
    let result = AssertUnwindSafe(body).catch_unwind().await;
    drop_tables(c, &f.schema, tables).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn col(name: &str, native: &str) -> ColumnDef {
    let mut c = ColumnDef::new(
        name,
        DataType::Other {
            name: native.to_owned(),
        },
    );
    native.clone_into(&mut c.native_type);
    c
}

fn index(name: &str, columns: Vec<IndexColumn>) -> IndexDef {
    IndexDef {
        name: name.to_owned(),
        columns,
        unique: false,
        is_constraint: false,
        predicate: None,
        extras: Extras::new(),
    }
}

fn expression(text: &str, descending: bool) -> IndexColumn {
    IndexColumn {
        name: text.to_owned(),
        is_expression: true,
        descending,
        length: None,
    }
}

fn simple_table(t: ObjectRef, columns: Vec<ColumnDef>) -> TableDef {
    let mut def = TableDef::new(t);
    def.columns = columns;
    def.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec!["id".into()],
    });
    def
}

fn quote(text: &str) -> String {
    PgQuoting.quote_str(text)
}

// --- round trips ----------------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn table_round_trip_create_alter_drop() {
    let f = fixture();
    let conn = connect(&f).await;
    let c = &*conn;
    let v = c.server_version().clone();
    let name = f.scratch("table", &token());
    let renamed = format!("{name}_r");
    let tables = [name.clone(), renamed.clone()];
    guarded(
        c,
        &f,
        &tables,
        Box::pin(async {
            let table = f.table_ref(&name);
            let mut def = TableDef::new(table.clone());
            let mut id = col("id", "integer").not_null();
            id.auto_increment = true;
            id.identity = Some(IdentityDef {
                always: false,
                start: None,
                increment: None,
            });
            def.columns.push(id);
            let mut select = col("select", "int").not_null();
            select.default = Some("0".into());
            def.columns.push(select);
            let mut code = col("na\"me ü", "varchar(20)").not_null();
            code.default = Some(quote("it's a \\ path ü 🚀"));
            code.comment = Some("Kunde's \"code\" ü \\ 🚀".into());
            def.columns.push(code);
            let mut note = col("note", "text");
            note.collation = Some("C".into());
            def.columns.push(note);
            let mut amount = col("amount", "decimal(12, 2)");
            amount.default = Some("1.50".into());
            def.columns.push(amount);
            let mut total = col("total", "numeric(14,2)");
            total.generated = Some(GeneratedColumn {
                expression: "amount * 2".into(),
                stored: true,
            });
            def.columns.push(total);
            def.columns.push(col("parent_id", "int4"));
            let mut created = col("created", "timestamp").not_null();
            created.default = Some("CURRENT_TIMESTAMP".into());
            def.columns.push(created);
            let mut flag = col("flag", "bool");
            flag.default = Some("true".into());
            def.columns.push(flag);
            let mut seq = col("seq", "bigint").not_null();
            seq.auto_increment = true;
            def.columns.push(seq);
            def.primary_key = Some(PrimaryKeyDef {
                name: Some(format!("{name}_pk")),
                columns: vec!["id".into()],
            });
            let mut unique = index(
                &format!("{name}_code_key"),
                vec![IndexColumn::column("na\"me ü")],
            );
            unique.unique = true;
            unique.is_constraint = true;
            def.indexes.push(unique);
            let mut lower = index(
                &format!("{name}_lower_idx"),
                vec![
                    expression("lower(note)", true),
                    IndexColumn::column("select"),
                ],
            );
            lower.predicate = Some("\"select\" > 0".into());
            def.indexes.push(lower);
            def.checks.push(CheckDef {
                name: Some(format!("{name}_chk")),
                expression: "\"select\" >= 0 AND note <> 'x''y'".into(),
                extras: Extras::new(),
            });
            def.foreign_keys.push(ForeignKeyDef {
                name: Some(format!("{name}_parent_fk")),
                columns: vec!["parent_id".into()],
                ref_table: ObjectRef::table(None, Some(&f.schema), name.as_str()),
                ref_columns: vec!["id".into()],
                on_delete: FkAction::SetNull,
                on_update: FkAction::NoAction,
                extras: Extras::new()
                    .with(props::DEFERRABLE, true)
                    .with(props::INITIALLY_DEFERRED, true),
            });
            def.comment = Some("O'Reilly \\ orders 🚀 ü".into());

            let source = ObjectDef::Table(def);
            let script = ddl_create(&source, &v, None);
            apply(c, &script).await;
            let fetched = load(c, &table).await;
            assert_same(&source, &fetched, &script);
            let ObjectDef::Table(old) = fetched else {
                unreachable!()
            };

            // Alter the fetched definition, as the designer does.
            let mut new = old.clone();
            new.obj.name.clone_from(&renamed);
            let renames = [
                Rename::new(ObjectKind::Table, &name, &renamed),
                Rename::new(ObjectKind::Column, "flag", "Flag ü"),
            ];
            let pos = |t: &TableDef, n: &str| {
                t.columns
                    .iter()
                    .position(|c| c.name == n)
                    .unwrap_or_else(|| panic!("no column {n}"))
            };
            let k = pos(&new, "flag");
            new.columns[k].name = "Flag ü".into();
            let k = pos(&new, "select");
            new.columns[k].native_type = "bigint".into();
            new.columns[k].default = Some("5".into());
            let k = pos(&new, "note");
            new.columns[k].nullable = false;
            let k = pos(&new, "created");
            new.columns.remove(k);
            let k = pos(&new, "id");
            new.columns[k].identity = Some(IdentityDef {
                always: true,
                start: Some(100),
                increment: Some(1),
            });
            let mut extra = col("extra", "text");
            extra.default = Some(quote(""));
            extra.comment = Some("added ü".into());
            new.columns.push(extra);
            new.comment = None;
            new.checks = vec![CheckDef {
                name: Some(format!("{name}_extra_chk")),
                expression: "extra <> 'bad'".into(),
                extras: Extras::new().with(NOT_VALID, true),
            }];
            for fk in &mut new.foreign_keys {
                fk.on_delete = FkAction::Cascade;
                fk.ref_table.name.clone_from(&renamed);
                fk.extras.insert(props::DEFERRABLE, false);
                fk.extras.insert(props::INITIALLY_DEFERRED, false);
            }
            for idx in &mut new.indexes {
                if idx.name.ends_with("_lower_idx") {
                    idx.predicate = Some("\"select\" > 1".into());
                }
            }
            let new_table = f.table_ref(&renamed);
            let source = ObjectDef::Table(new);
            let old_def = ObjectDef::Table(old.clone());
            let script = ddl_alter(&old_def, &source, &v, Some(&old), &renames);
            assert!(!script.is_empty());
            assert!(script.is_destructive(), "drops a column: {script}");
            apply(c, &script).await;
            let fetched = load(c, &new_table).await;
            assert_same(&source, &fetched, &script);
            // Nothing left to change.
            let again = ddl_alter(&fetched, &source, &v, None, &[]);
            assert!(again.is_empty(), "{again}");

            let script = ddl_drop(&new_table, &v, None);
            apply(c, &script).await;
            assert_gone(c, &new_table).await;
        }),
    )
    .await;
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn column_round_trip_create_alter_drop() {
    let f = fixture();
    let conn = connect(&f).await;
    let c = &*conn;
    let v = c.server_version().clone();
    let name = f.scratch("column", &token());
    guarded(
        c,
        &f,
        std::slice::from_ref(&name),
        Box::pin(async {
            let table = f.table_ref(&name);
            let base = simple_table(table.clone(), vec![col("id", "integer").not_null()]);
            apply(c, &ddl_create(&ObjectDef::Table(base), &v, None)).await;
            let current = load_table(c, &table).await;

            let mut code = col("code", "varchar(10)").not_null();
            code.default = Some(quote("a'b\\ ü"));
            code.comment = Some("c'mt \\ ü".into());
            let source = ObjectDef::Column {
                table: table.clone(),
                column: code,
            };
            let script = ddl_create(&source, &v, Some(&current));
            apply(c, &script).await;
            let code_ref = table.child(ObjectKind::Column, "code");
            let fetched = load(c, &code_ref).await;
            assert_same(&source, &fetched, &script);

            let ObjectDef::Column { column: before, .. } = &fetched else {
                unreachable!()
            };
            let mut after = before.clone();
            after.name = "Code Ü".into();
            after.native_type = "text".into();
            after.nullable = true;
            after.default = Some(quote("z"));
            after.comment = None;
            let source = ObjectDef::Column {
                table: table.clone(),
                column: after,
            };
            let current = load_table(c, &table).await;
            let script = ddl_alter(&fetched, &source, &v, Some(&current), &[]);
            assert!(!script.is_destructive(), "varchar -> text widens: {script}");
            apply(c, &script).await;
            let renamed = table.child(ObjectKind::Column, "Code Ü");
            let fetched = load(c, &renamed).await;
            assert_same(&source, &fetched, &script);
            assert_gone(c, &code_ref).await;

            // A plain integer column made an identity column.
            let n = ObjectDef::Column {
                table: table.clone(),
                column: col("n", "integer"),
            };
            apply(c, &ddl_create(&n, &v, None)).await;
            let n_ref = table.child(ObjectKind::Column, "n");
            let before = load(c, &n_ref).await;
            let mut ident = col("n", "integer").not_null();
            ident.auto_increment = true;
            ident.identity = Some(IdentityDef {
                always: false,
                start: Some(5),
                increment: Some(2),
            });
            let source = ObjectDef::Column {
                table: table.clone(),
                column: ident,
            };
            let script = ddl_alter(&before, &source, &v, None, &[]);
            apply(c, &script).await;
            assert_same(&source, &load(c, &n_ref).await, &script);

            let script = ddl_drop(&renamed, &v, None);
            assert!(script.is_destructive());
            apply(c, &script).await;
            assert_gone(c, &renamed).await;
        }),
    )
    .await;
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn index_round_trip_create_alter_drop() {
    let f = fixture();
    let conn = connect(&f).await;
    let c = &*conn;
    let v = c.server_version().clone();
    let name = f.scratch("index", &token());
    guarded(
        c,
        &f,
        std::slice::from_ref(&name),
        Box::pin(async {
            let table = f.table_ref(&name);
            let base = simple_table(
                table.clone(),
                vec![
                    col("id", "integer").not_null(),
                    col("code", "varchar(20)").not_null(),
                    col("qty", "integer"),
                ],
            );
            apply(c, &ddl_create(&ObjectDef::Table(base), &v, None)).await;

            let mut idx = index(
                &format!("{name}_idx"),
                vec![expression("lower(code)", true), IndexColumn::column("qty")],
            );
            idx.predicate = Some("qty > 0".into());
            idx.extras.insert(props::INDEX_METHOD, "btree");
            idx.extras.insert(
                props::INCLUDE_COLUMNS,
                Value::Json(serde_json::json!(["id"])),
            );
            let source = ObjectDef::Index {
                table: table.clone(),
                index: idx,
            };
            let script = ddl_create(&source, &v, None);
            apply(c, &script).await;
            let idx_ref = table.child(ObjectKind::Index, format!("{name}_idx"));
            let fetched = load(c, &idx_ref).await;
            assert_same(&source, &fetched, &script);

            // Renamed, unique, NULLS NOT DISTINCT, no predicate.
            let mut unique = index(&format!("{name}_uidx"), vec![IndexColumn::column("code")]);
            unique.unique = true;
            unique
                .extras
                .insert(oxbase_pg::dialect::NULLS_NOT_DISTINCT, true);
            let source = ObjectDef::Index {
                table: table.clone(),
                index: unique.clone(),
            };
            let script = ddl_alter(&fetched, &source, &v, None, &[]);
            apply(c, &script).await;
            let uidx_ref = table.child(ObjectKind::Index, format!("{name}_uidx"));
            let fetched = load(c, &uidx_ref).await;
            assert_same(&source, &fetched, &script);
            assert_gone(c, &idx_ref).await;

            // The same as a declared UNIQUE constraint.
            unique.is_constraint = true;
            let source = ObjectDef::Index {
                table: table.clone(),
                index: unique,
            };
            let script = ddl_alter(&fetched, &source, &v, None, &[]);
            apply(c, &script).await;
            let fetched = load(c, &uidx_ref).await;
            assert_same(&source, &fetched, &script);

            let current = load_table(c, &table).await;
            let script = ddl_drop(&uidx_ref, &v, Some(&current));
            assert!(script.steps[0].sql.contains("DROP CONSTRAINT"), "{script}");
            apply(c, &script).await;
            assert_gone(c, &uidx_ref).await;
        }),
    )
    .await;
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn foreign_key_round_trip_create_alter_drop() {
    let f = fixture();
    let conn = connect(&f).await;
    let c = &*conn;
    let v = c.server_version().clone();
    let tok = token();
    let name = f.scratch("foreign_key", &tok);
    let parent = format!("{}fk_{tok}_ref", f.prefix);
    let tables = [name.clone(), parent.clone()];
    guarded(
        c,
        &f,
        &tables,
        Box::pin(async {
            let parent_ref = f.table_ref(&parent);
            let mut parent_def = simple_table(
                parent_ref.clone(),
                vec![
                    col("id", "integer").not_null(),
                    col("a", "integer"),
                    col("b", "text"),
                ],
            );
            let mut ab = index(
                &format!("{parent}_ab_key"),
                vec![IndexColumn::column("a"), IndexColumn::column("b")],
            );
            ab.unique = true;
            ab.is_constraint = true;
            parent_def.indexes.push(ab);
            apply(c, &ddl_create(&ObjectDef::Table(parent_def), &v, None)).await;
            let table = f.table_ref(&name);
            let child = simple_table(
                table.clone(),
                vec![
                    col("id", "integer").not_null(),
                    col("a", "integer"),
                    col("b", "text"),
                ],
            );
            apply(c, &ddl_create(&ObjectDef::Table(child), &v, None)).await;

            let fk = ForeignKeyDef {
                name: Some(format!("{name}_fk")),
                columns: vec!["a".into(), "b".into()],
                ref_table: ObjectRef::table(None, Some(&f.schema), parent.as_str()),
                ref_columns: vec!["a".into(), "b".into()],
                on_delete: FkAction::NoAction,
                on_update: FkAction::Cascade,
                extras: Extras::new()
                    .with(oxbase_pg::dialect::MATCH_TYPE, "full")
                    .with(NOT_VALID, true),
            };
            let def = |fk: &ForeignKeyDef| ObjectDef::ForeignKey {
                table: table.clone(),
                foreign_key: fk.clone(),
            };
            let script = ddl_create(&def(&fk), &v, None);
            apply(c, &script).await;
            let fk_ref = table.child(ObjectKind::ForeignKey, format!("{name}_fk"));
            let mut fetched = load(c, &fk_ref).await;
            assert_same(&def(&fk), &fetched, &script);

            // Validate, then make deferrable, then rename and change the action.
            let mut validated = fk.clone();
            validated.extras.insert(NOT_VALID, false);
            let mut deferred = validated.clone();
            deferred.extras.insert(props::DEFERRABLE, true);
            deferred.extras.insert(props::INITIALLY_DEFERRED, true);
            let mut changed = deferred.clone();
            changed.name = Some(format!("{name}_fk2"));
            changed.on_delete = FkAction::SetNull;
            for (next, expected_steps, sql) in [
                (&validated, 1, "VALIDATE CONSTRAINT"),
                (&deferred, 1, "ALTER CONSTRAINT"),
                (&changed, 3, "RENAME CONSTRAINT"),
            ] {
                let script = ddl_alter(&fetched, &def(next), &v, None, &[]);
                assert_eq!(script.len(), expected_steps, "{script}");
                assert!(script.steps[0].sql.contains(sql), "{script}");
                apply(c, &script).await;
                let at = table.child(ObjectKind::ForeignKey, next.name.clone().expect("named"));
                fetched = load(c, &at).await;
                assert_same(&def(next), &fetched, &script);
            }
            assert_gone(c, &fk_ref).await;

            let fk2_ref = table.child(ObjectKind::ForeignKey, format!("{name}_fk2"));
            let script = ddl_drop(&fk2_ref, &v, None);
            apply(c, &script).await;
            assert_gone(c, &fk2_ref).await;
        }),
    )
    .await;
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn check_round_trip_create_alter_drop() {
    let f = fixture();
    let conn = connect(&f).await;
    let c = &*conn;
    let v = c.server_version().clone();
    let name = f.scratch("check", &token());
    guarded(
        c,
        &f,
        std::slice::from_ref(&name),
        Box::pin(async {
            let table = f.table_ref(&name);
            let base = simple_table(
                table.clone(),
                vec![
                    col("id", "integer").not_null(),
                    col("qty", "integer"),
                    col("note", "varchar(10)"),
                ],
            );
            apply(c, &ddl_create(&ObjectDef::Table(base), &v, None)).await;

            let check = CheckDef {
                name: Some(format!("{name}_chk")),
                expression: "qty >= 0 AND note <> '' AND char_length(note) < 5".into(),
                extras: Extras::new(),
            };
            let def = |check: &CheckDef| ObjectDef::Check {
                table: table.clone(),
                check: check.clone(),
            };
            let script = ddl_create(&def(&check), &v, None);
            apply(c, &script).await;
            let chk_ref = table.child(ObjectKind::Check, format!("{name}_chk"));
            let fetched = load(c, &chk_ref).await;
            assert_same(&def(&check), &fetched, &script);

            let mut changed = check.clone();
            changed.name = Some(format!("{name}_chk2"));
            changed.expression = "qty > 0".into();
            changed.extras.insert(oxbase_pg::dialect::NO_INHERIT, true);
            let script = ddl_alter(&fetched, &def(&changed), &v, None, &[]);
            apply(c, &script).await;
            let chk2_ref = table.child(ObjectKind::Check, format!("{name}_chk2"));
            let fetched = load(c, &chk2_ref).await;
            assert_same(&def(&changed), &fetched, &script);
            assert_gone(c, &chk_ref).await;

            let script = ddl_drop(&chk2_ref, &v, None);
            apply(c, &script).await;
            assert_gone(c, &chk2_ref).await;
        }),
    )
    .await;
}

// --- grid writes ----------------------------------------------------------------------------

/// Plans `edit` for `table` keyed by `key`, reading back `read_back`.
fn plan(
    table: &TableDef,
    key: &RowKey,
    v: &ServerVersion,
    read_back: &[String],
    edit: &RowEdit,
) -> oxbase_dialect::DmlPlan {
    registry()
        .dml_writer(Engine::Postgres)
        .expect("dml writer")
        .plan(
            edit,
            &DmlTarget {
                table,
                key,
                version: v,
                read_back,
            },
        )
        .unwrap_or_else(|e| panic!("plan {edit:?}: {e}"))
}

/// Rows a write touched: rows returned when it returns them, else the affected count.
fn touched(out: &Collected, returning: bool) -> u64 {
    if returning {
        u64::try_from(returned_rows(out)).expect("count")
    } else {
        out.rows_affected.iter().sum()
    }
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn dml_writes_bound_values_and_reads_rows_back() {
    let f = fixture();
    let conn = connect(&f).await;
    let c = &*conn;
    let v = c.server_version().clone();
    let tok = token();
    let table_ref = f.table_ref(&f.edit_pk);
    let table = load_table(c, &table_ref).await;
    let key = RowKey::new(table_ref.clone(), KeyKind::Primary, vec!["id".into()]);
    let read_back: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let cleanup = format!("DELETE FROM m3.{} WHERE code LIKE 't{tok}-%'", f.edit_pk);
    let result = AssertUnwindSafe(Box::pin(async {
        // Insert with every kind of value, in one transaction like the edit service.
        let insert = RowEdit::Insert {
            values: vec![
                ColumnValue::new("code", format!("t{tok}-1")),
                ColumnValue::new("name", "it's \\ ü 🚀; DROP TABLE x"),
                ColumnValue::new("amount", Value::Decimal("12.50".into())),
                ColumnValue::new("flag", true),
                ColumnValue::new("doc", Value::Json(serde_json::json!({"a": [1, "b"]}))),
                ColumnValue::new("data", Value::Bytes(vec![0, 1, 0xfe])),
            ],
        };
        let p = plan(&table, &key, &v, &read_back, &insert);
        assert!(p.returning);
        assert!(!p.write.sql.contains("DROP"), "{}", p.write.sql);
        let tx = c.begin().await.expect("begin");
        let out = collect(
            tx.execute(&p.write.sql, &p.write.params, CancellationToken::new())
                .await
                .expect("insert"),
        )
        .await
        .expect("insert");
        assert_eq!(touched(&out, p.returning), 1);
        // The identity key and the generated column come back with the insert.
        let set = &out.result_sets[0];
        let id_col = read_back.iter().position(|n| n == "id").expect("id");
        let ids = set.batches[0]
            .column(id_col)
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("int4 id");
        let id = ids.value(0);
        assert!(id > 3, "a generated key after the seed rows: {id}");
        let total_col = read_back.iter().position(|n| n == "total").expect("total");
        assert!(!set.batches[0].column(total_col).is_null(0));
        tx.commit().await.expect("commit");
        let row = texts(
            &run(
                c,
                "SELECT total::text, name, doc::text, encode(data, 'hex') \
                 FROM m3.m3_edit_pk WHERE id = $1",
                &[Value::Int(i64::from(id))],
            )
            .await,
        );
        assert_eq!(
            row,
            [[
                Some("25.00".to_owned()),
                Some("it's \\ ü 🚀; DROP TABLE x".to_owned()),
                Some("{\"a\": [1, \"b\"]}".to_owned()),
                Some("0001fe".to_owned()),
            ]]
        );

        // NULL and the empty string stay different.
        let tx = c.begin().await.expect("begin");
        for (n, name) in [(2, Value::Text(String::new())), (3, Value::Null)] {
            let p = plan(
                &table,
                &key,
                &v,
                &read_back,
                &RowEdit::Insert {
                    values: vec![
                        ColumnValue::new("code", format!("t{tok}-{n}")),
                        ColumnValue::new("name", name),
                    ],
                },
            );
            let out = collect(
                tx.execute(&p.write.sql, &p.write.params, CancellationToken::new())
                    .await
                    .expect("insert"),
            )
            .await
            .expect("insert");
            assert_eq!(touched(&out, p.returning), 1);
        }
        tx.commit().await.expect("commit");
        let rows = texts(
            &run(
                c,
                "SELECT code::text, (name IS NULL)::text, (name = '')::text FROM m3.m3_edit_pk \
                 WHERE code IN ($1, $2) ORDER BY code",
                &[
                    Value::Text(format!("t{tok}-2")),
                    Value::Text(format!("t{tok}-3")),
                ],
            )
            .await,
        );
        assert_eq!(
            rows,
            [
                [
                    Some(format!("t{tok}-2")),
                    Some("false".to_owned()),
                    Some("true".to_owned())
                ],
                [Some(format!("t{tok}-3")), Some("true".to_owned()), None],
            ]
        );

        // Update by key, including the key itself: the row comes back under its new key.
        let new_id = i64::from(id) + 1_000_000;
        let update = RowEdit::Update {
            key: vec![Value::Int(i64::from(id))],
            changes: vec![
                ColumnValue::new("name", Value::Null),
                ColumnValue::new("id", new_id),
            ],
        };
        let p = plan(&table, &key, &v, &read_back, &update);
        let out = run(c, &p.write.sql, &p.write.params).await;
        assert_eq!(touched(&out, p.returning), 1);
        let ids = out.result_sets[0].batches[0]
            .column(id_col)
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("int4 id");
        assert_eq!(i64::from(ids.value(0)), new_id);

        // Delete by key.
        let delete = RowEdit::Delete {
            key: vec![Value::Int(new_id)],
        };
        let p = plan(&table, &key, &v, &read_back, &delete);
        assert!(!p.returning);
        let out = run(c, &p.write.sql, &p.write.params).await;
        assert_eq!(touched(&out, p.returning), 1);
        // Seed rows are untouched.
        let seeds = texts(
            &run(
                c,
                "SELECT count(*)::text FROM m3.m3_edit_pk WHERE id IN (1, 2, 3)",
                &[],
            )
            .await,
        );
        assert_eq!(seeds, [[Some("3".to_owned())]]);
    }))
    .catch_unwind()
    .await;
    run(c, &cleanup, &[]).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn dml_conflicts_when_the_row_changed_or_vanished() {
    let f = fixture();
    let conn = connect(&f).await;
    let other = connect(&f).await;
    let c = &*conn;
    let v = c.server_version().clone();
    let tok = token();
    let table_ref = f.table_ref(&f.edit_uq);
    let table = load_table(c, &table_ref).await;
    let key = RowKey::new(table_ref.clone(), KeyKind::Unique, vec!["code".into()]);
    let read_back = ["code".to_owned(), "val".to_owned()];
    let cleanup = format!("DELETE FROM m3.{} WHERE code LIKE 't{tok}-%'", f.edit_uq);
    let result = AssertUnwindSafe(Box::pin(async {
        let code = format!("t{tok}-u");
        let p = plan(
            &table,
            &key,
            &v,
            &read_back,
            &RowEdit::Insert {
                values: vec![
                    ColumnValue::new("code", code.clone()),
                    ColumnValue::new("val", 1),
                ],
            },
        );
        assert_eq!(
            touched(&run(c, &p.write.sql, &p.write.params).await, true),
            1
        );

        // A no-op update still counts the row.
        let same = RowEdit::Update {
            key: vec![Value::Text(code.clone())],
            changes: vec![ColumnValue::new("val", 1)],
        };
        let p = plan(&table, &key, &v, &read_back, &same);
        assert_eq!(
            touched(&run(c, &p.write.sql, &p.write.params).await, p.returning),
            1
        );

        // Another session changes the key: the update by the old key touches no row, and the
        // edit service rolls back.
        run(
            &*other,
            "UPDATE m3.m3_edit_uq SET code = $1 WHERE code = $2",
            &[Value::Text(format!("{code}2")), Value::Text(code.clone())],
        )
        .await;
        let update = RowEdit::Update {
            key: vec![Value::Text(code.clone())],
            changes: vec![ColumnValue::new("val", 2)],
        };
        let p = plan(&table, &key, &v, &read_back, &update);
        let tx = c.begin().await.expect("begin");
        let out = collect(
            tx.execute(&p.write.sql, &p.write.params, CancellationToken::new())
                .await
                .expect("update"),
        )
        .await
        .expect("update");
        assert_eq!(touched(&out, p.returning), 0, "conflict: the row changed");
        tx.rollback().await.expect("rollback");

        // Another session deletes the row: the delete touches no row.
        run(
            &*other,
            "DELETE FROM m3.m3_edit_uq WHERE code = $1",
            &[Value::Text(format!("{code}2"))],
        )
        .await;
        let delete = RowEdit::Delete {
            key: vec![Value::Text(format!("{code}2"))],
        };
        let p = plan(&table, &key, &v, &read_back, &delete);
        assert_eq!(
            touched(&run(c, &p.write.sql, &p.write.params).await, p.returning),
            0
        );
    }))
    .catch_unwind()
    .await;
    run(c, &cleanup, &[]).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn dml_defaults_identity_and_cast_values() {
    let f = fixture();
    let conn = connect(&f).await;
    let c = &*conn;
    let v = c.server_version().clone();
    let name = format!("{}_d", f.scratch("table", &token()));
    guarded(
        c,
        &f,
        std::slice::from_ref(&name),
        Box::pin(async {
            let table_ref = f.table_ref(&name);
            let mut id = col("id", "integer").not_null();
            id.auto_increment = true;
            id.identity = Some(IdentityDef {
                always: true,
                start: None,
                increment: None,
            });
            let mut note = col("note", "text");
            note.default = Some(quote("n'x"));
            let def = simple_table(
                table_ref.clone(),
                vec![id, note, col("tags", "text[]"), col("span", "interval")],
            );
            apply(c, &ddl_create(&ObjectDef::Table(def), &v, None)).await;
            let table = load_table(c, &table_ref).await;
            let key = RowKey::new(table_ref.clone(), KeyKind::Primary, vec!["id".into()]);
            let read_back = [
                "id".to_owned(),
                "note".to_owned(),
                "tags".to_owned(),
                "span".to_owned(),
            ];

            // A row of defaults returns its generated key and default.
            let p = plan(
                &table,
                &key,
                &v,
                &read_back,
                &RowEdit::Insert { values: vec![] },
            );
            assert!(p.write.sql.contains("DEFAULT VALUES"), "{}", p.write.sql);
            let out = run(c, &p.write.sql, &p.write.params).await;
            assert_eq!(touched(&out, p.returning), 1);
            let batch = &out.result_sets[0].batches[0];
            let ids = batch
                .column(0)
                .as_any()
                .downcast_ref::<Int32Array>()
                .expect("int4 id");
            assert_eq!(ids.value(0), 1);
            let notes = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("text");
            assert_eq!(notes.value(0), "n'x");

            // Arrays and intervals arrive as text and are cast in SQL.
            let insert = RowEdit::Insert {
                values: vec![
                    ColumnValue::new("tags", "{a,\"b c\"}"),
                    ColumnValue::new("span", "1 day 02:00:00"),
                ],
            };
            let p = plan(&table, &key, &v, &read_back, &insert);
            assert!(
                p.write.sql.contains("CAST($1::text AS text[])"),
                "{}",
                p.write.sql
            );
            let out = run(c, &p.write.sql, &p.write.params).await;
            assert_eq!(touched(&out, p.returning), 1);
            let qualified =
                PgQuoting.qualify(&ObjectRef::table(None, Some(&f.schema), name.as_str()));
            let rows = texts(
                &run(
                    c,
                    &format!("SELECT tags::text, span::text FROM {qualified} WHERE id = 2"),
                    &[],
                )
                .await,
            );
            assert_eq!(
                rows,
                [[
                    Some("{a,\"b c\"}".to_owned()),
                    Some("1 day 02:00:00".to_owned())
                ]]
            );
            // The GENERATED ALWAYS identity cannot be written.
            let err = registry()
                .dml_writer(Engine::Postgres)
                .expect("dml writer")
                .plan(
                    &RowEdit::Insert {
                        values: vec![ColumnValue::new("id", 5)],
                    },
                    &DmlTarget {
                        table: &table,
                        key: &key,
                        version: &v,
                        read_back: &read_back,
                    },
                )
                .unwrap_err();
            assert!(matches!(err, Error::InvalidInput { .. }), "{err}");
        }),
    )
    .await;
}
