//! M3 SQLite cells against real SQLite databases: the `DdlGenerator` scripts (native changes
//! and the table rebuild) applied and re-fetched with the M2 fetchers, and the `DmlWriter`
//! plans run with their bound values.
//!
//! The tests without `#[ignore]` use in-memory databases. The `#[ignore]`d ones are the live
//! tests the CI conformance job runs (`--include-ignored`): they work on the fixture, a fresh
//! temporary database file seeded with `tests/docker/init/sqlite/01-m2.sql` and `02-m3.sql`
//! (`tests/fixtures.toml` `[m2.sqlite]` / `[m3.sqlite]`), with the `[m3]` scratch names.
//!
//! Scripts run the way `oxbase_types::script` describes (prepare steps outside the transaction,
//! main steps in one transaction, finally steps always; captures, conditions and
//! `expect_no_rows`), by the small executor below.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arrow_array::Array;
use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int64Type};
use arrow_schema::DataType as ArrowType;
use chrono::{NaiveDate, NaiveDateTime};
use oxbase_dialect::{
    AnsiQuoting, DdlContext, DdlGenerator, DialectRegistry, DmlPlan, DmlTarget, DmlWriter, Fetcher,
    Quoting,
};
use oxbase_driver::{CancellationToken, Collected, ConnectParams, Connection, Driver, Tx, collect};
use oxbase_sqlite::SqliteDriver;
use oxbase_sqlite::dialect::{self, REBUILD_SUFFIX};
use oxbase_types::{
    CheckDef, ColumnDef, ColumnValue, DataType, Engine, Error, ErrorCategory, Extras, FkAction,
    ForeignKeyDef, IndexColumn, IndexDef, KeyKind, ObjectDef, ObjectKind, ObjectRef, PrimaryKeyDef,
    PropertyDiff, Rename, Result, RowEdit, RowKey, Script, ScriptStep, StepCheck, StepPhase,
    TableDef, TriggerDef, Value,
};
use tempfile::TempDir;

const M2_SEED: &str = include_str!("../../../tests/docker/init/sqlite/01-m2.sql");
const M3_SEED: &str = include_str!("../../../tests/docker/init/sqlite/02-m3.sql");
const FIXTURES: &str = include_str!("../../../tests/fixtures.toml");

// --- plumbing ------------------------------------------------------------------------------

fn registry() -> DialectRegistry {
    let mut reg = DialectRegistry::new();
    dialect::register(&mut reg);
    reg
}

fn ddl(kind: ObjectKind) -> Arc<dyn DdlGenerator> {
    registry().ddl(Engine::Sqlite, kind).unwrap()
}

fn writer() -> Arc<dyn DmlWriter> {
    registry().dml_writer(Engine::Sqlite).unwrap()
}

fn fetcher(kind: ObjectKind) -> Arc<dyn Fetcher> {
    registry().fetcher(Engine::Sqlite, kind).unwrap()
}

async fn connect(path: &Path) -> Box<dyn Connection> {
    SqliteDriver
        .connect(&ConnectParams::sqlite(path))
        .await
        .unwrap()
}

async fn memory() -> Box<dyn Connection> {
    connect(Path::new(":memory:")).await
}

/// A fresh fixture file with the M2 and M3 seeds.
async fn fixture() -> (TempDir, Box<dyn Connection>) {
    let dir = tempfile::tempdir().unwrap();
    let c = connect(&dir.path().join("oxbase_test.sqlite")).await;
    run(c.as_ref(), M2_SEED).await;
    run(c.as_ref(), M3_SEED).await;
    (dir, c)
}

async fn exec_on(c: &dyn Connection, sql: &str, params: &[Value]) -> Result<Collected> {
    collect(c.execute(sql, params, CancellationToken::new()).await?).await
}

async fn exec_tx(tx: &dyn Tx, sql: &str, params: &[Value]) -> Result<Collected> {
    collect(tx.execute(sql, params, CancellationToken::new()).await?).await
}

async fn run(c: &dyn Connection, sql: &str) {
    exec_on(c, sql, &[])
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn cell(a: &dyn Array, row: usize) -> Option<String> {
    if a.is_null(row) {
        return None;
    }
    Some(match a.data_type() {
        ArrowType::Int64 => a.as_primitive::<Int64Type>().value(row).to_string(),
        ArrowType::Float64 => a.as_primitive::<Float64Type>().value(row).to_string(),
        ArrowType::Utf8 => a.as_string::<i32>().value(row).to_owned(),
        ArrowType::Binary => hex(a.as_binary::<i32>().value(row)),
        other => format!("<{other}>"),
    })
}

type Rows = Vec<Vec<Option<String>>>;

fn rows_of(got: &Collected) -> Rows {
    let mut out = Vec::new();
    for set in &got.result_sets {
        for batch in &set.batches {
            for row in 0..batch.num_rows() {
                out.push(
                    batch
                        .columns()
                        .iter()
                        .map(|a| cell(a.as_ref(), row))
                        .collect(),
                );
            }
        }
    }
    out
}

async fn query(c: &dyn Connection, sql: &str) -> Rows {
    rows_of(&exec_on(c, sql, &[]).await.unwrap())
}

/// The first value of the first row.
async fn scalar(c: &dyn Connection, sql: &str) -> Option<String> {
    query(c, sql)
        .await
        .into_iter()
        .next()
        .and_then(|row| row.into_iter().next().flatten())
}

fn some(values: &[&str]) -> Vec<Option<String>> {
    values.iter().map(|v| Some((*v).to_owned())).collect()
}

// --- the script executor -------------------------------------------------------------------

enum On<'a> {
    Conn(&'a dyn Connection),
    Tx(&'a dyn Tx),
}

async fn run_step(
    on: &On<'_>,
    step: &ScriptStep,
    captures: &mut HashMap<String, String>,
) -> Result<()> {
    if let Some(cond) = &step.only_if
        && captures.get(&cond.capture).map(String::as_str) != Some(cond.equals.as_str())
    {
        return Ok(());
    }
    let got = match on {
        On::Conn(c) => exec_on(*c, &step.sql, &[]).await?,
        On::Tx(tx) => exec_tx(*tx, &step.sql, &[]).await?,
    };
    let rows = rows_of(&got);
    if let Some(name) = &step.capture {
        let first = rows
            .first()
            .and_then(|row| row.first().cloned().flatten())
            .unwrap_or_default();
        captures.insert(name.clone(), first);
    }
    if step.check == StepCheck::ExpectNoRows && !rows.is_empty() {
        return Err(Error::invalid_input(format!(
            "{}: {} row(s), first {:?}",
            step.description,
            rows.len(),
            rows[0]
        )));
    }
    Ok(())
}

async fn run_main(
    c: &dyn Connection,
    script: &Script,
    captures: &mut HashMap<String, String>,
) -> Result<()> {
    let tx = c.begin().await?;
    for step in script.steps.iter().filter(|s| s.phase == StepPhase::Main) {
        let outcome = run_step(&On::Tx(tx.as_ref()), step, captures).await;
        if let Err(e) = outcome {
            tx.rollback().await?;
            return Err(e);
        }
    }
    tx.commit().await
}

/// Runs `script` as `oxbase_types::script` describes.
async fn apply(c: &dyn Connection, script: &Script) -> Result<()> {
    let mut captures = HashMap::new();
    let mut outcome = Ok(());
    for step in script
        .steps
        .iter()
        .filter(|s| s.phase == StepPhase::Prepare)
    {
        if let Err(e) = run_step(&On::Conn(c), step, &mut captures).await {
            outcome = Err(e);
            break;
        }
    }
    if outcome.is_ok() {
        outcome = run_main(c, script, &mut captures).await;
    }
    for step in script
        .steps
        .iter()
        .filter(|s| s.phase == StepPhase::Finally)
    {
        run_step(&On::Conn(c), step, &mut captures)
            .await
            .unwrap_or_else(|e| panic!("finally step {}: {e}", step.sql));
    }
    outcome
}

// --- definitions ---------------------------------------------------------------------------

fn table_ref(name: &str) -> ObjectRef {
    ObjectRef::table(Some("main"), None, name)
}

async fn load_table(c: &dyn Connection, name: &str) -> TableDef {
    match fetcher(ObjectKind::Table)
        .load(c, &table_ref(name), CancellationToken::new())
        .await
        .unwrap()
    {
        ObjectDef::Table(t) => t,
        other => panic!("not a table: {other:?}"),
    }
}

async fn table_gone(c: &dyn Connection, name: &str) -> bool {
    fetcher(ObjectKind::Table)
        .load(c, &table_ref(name), CancellationToken::new())
        .await
        .is_err_and(|e| e.category() == ErrorCategory::NotFound)
}

async fn triggers_of(c: &dyn Connection, table: &str) -> Vec<TriggerDef> {
    let f = fetcher(ObjectKind::Trigger);
    let mut out = Vec::new();
    for s in f
        .list(c, &table_ref(table), CancellationToken::new())
        .await
        .unwrap()
    {
        if let ObjectDef::Trigger(t) = f.load(c, &s.obj, CancellationToken::new()).await.unwrap() {
            out.push(t);
        }
    }
    out
}

fn diff_tables(expected: &TableDef, fetched: &TableDef) -> Vec<PropertyDiff> {
    registry()
        .comparator(Engine::Sqlite, ObjectKind::Table)
        .unwrap()
        .diff(
            &ObjectDef::Table(expected.clone()),
            &ObjectDef::Table(fetched.clone()),
        )
}

fn assert_same(expected: &TableDef, fetched: &TableDef) {
    assert_eq!(
        diff_tables(expected, fetched),
        Vec::<PropertyDiff>::new(),
        "\nexpected: {expected:#?}\nfetched: {fetched:#?}"
    );
}

fn column(name: &str, native: &str, data_type: DataType) -> ColumnDef {
    let mut c = ColumnDef::new(name, data_type);
    native.clone_into(&mut c.native_type);
    c
}

fn id_column() -> ColumnDef {
    let mut id = column("id", "INTEGER", DataType::INT64).not_null();
    id.auto_increment = true;
    id
}

fn pk(columns: &[&str]) -> PrimaryKeyDef {
    PrimaryKeyDef {
        name: None,
        columns: columns.iter().map(|c| (*c).to_owned()).collect(),
    }
}

fn index(name: &str, columns: &[&str]) -> IndexDef {
    IndexDef {
        name: name.to_owned(),
        columns: columns.iter().map(|c| IndexColumn::column(*c)).collect(),
        unique: false,
        is_constraint: false,
        predicate: None,
        extras: Extras::new(),
    }
}

fn check(name: &str, expression: &str) -> CheckDef {
    CheckDef {
        name: Some(name.to_owned()),
        expression: expression.to_owned(),
        extras: Extras::new(),
    }
}

async fn alter(
    c: &dyn Connection,
    old: &TableDef,
    new: &TableDef,
    renames: &[Rename],
) -> Result<()> {
    let triggers = triggers_of(c, &old.obj.name).await;
    let ctx = DdlContext::new(c.server_version())
        .with_table(old)
        .with_triggers(&triggers)
        .with_renames(renames);
    let script = ddl(ObjectKind::Table).alter(
        &ObjectDef::Table(old.clone()),
        &ObjectDef::Table(new.clone()),
        &ctx,
    )?;
    apply(c, &script).await
}

/// Ten lowercase base-36 characters from the time and the process id (the `[m3]` token).
fn token() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut n = u128::from(secs) * 4_194_304 + u128::from(std::process::id());
    let mut out = String::new();
    for _ in 0..10 {
        let digit = u32::try_from(n % 36).unwrap();
        out.insert(0, char::from_digit(digit, 36).unwrap());
        n /= 36;
    }
    out
}

// --- registration and fixtures -------------------------------------------------------------

#[test]
fn registers_every_m3_role() {
    let reg = registry();
    assert_eq!(
        reg.missing_designer_roles(Engine::Sqlite),
        Vec::<String>::new()
    );
    let caps = reg
        .capabilities(Engine::Sqlite)
        .unwrap()
        .designer(&"3.51.0".parse().unwrap());
    assert!(caps.transactional_ddl && caps.create_table);
    for line in [
        r#"init_sql = "tests/docker/init/sqlite/02-m3.sql""#,
        r#"edit_pk_table = "m3_edit_pk""#,
        r#"edit_unique_table = "m3_edit_uq""#,
        r#"edit_nokey_table = "m3_edit_nokey""#,
        r#"edit_view = "m3_edit_view""#,
        r#"roundtrip_prefix = "m3_rt_""#,
    ] {
        assert!(FIXTURES.contains(line), "fixtures.toml lost: {line}");
    }
    assert_eq!(token().len(), 10);
}

// --- rebuilds on in-memory databases -------------------------------------------------------

#[tokio::test]
async fn rebuild_keeps_rows_indexes_triggers_and_views() {
    let c = memory().await;
    let c = c.as_ref();
    run(
        c,
        "PRAGMA foreign_keys = ON;
         CREATE TABLE t (id INTEGER PRIMARY KEY, code TEXT NOT NULL UNIQUE,
                         qty INTEGER DEFAULT 1, note TEXT);
         CREATE INDEX t_qty ON t (qty);
         CREATE TABLE log (msg TEXT);
         CREATE TRIGGER t_ins AFTER INSERT ON t BEGIN
             INSERT INTO log VALUES ('ins ' || NEW.code);
         END;
         CREATE VIEW t_view AS SELECT code, qty FROM t;
         INSERT INTO t (id, code, qty, note) VALUES (1, 'a', 5, 'x'), (2, 'b', 7, NULL);",
    )
    .await;
    let old = load_table(c, "t").await;
    let mut new = old.clone();
    new.columns[2].native_type = "REAL".into();
    new.columns[2].data_type = DataType::FLOAT64;
    new.columns.retain(|col| col.name != "note");
    let mut added = column("added", "TEXT", DataType::Text);
    added.default = Some(AnsiQuoting.quote_str("n"));
    new.columns.push(added);
    new.checks.push(check("t_qty_chk", "qty >= 0"));
    alter(c, &old, &new, &[]).await.unwrap();

    assert_same(&new, &load_table(c, "t").await);
    assert_eq!(
        query(c, "SELECT id, code, qty, added FROM t ORDER BY id").await,
        [some(&["1", "a", "5", "n"]), some(&["2", "b", "7", "n"])]
    );
    assert_eq!(
        query(
            c,
            "SELECT name FROM sqlite_schema WHERE type = 'index' AND tbl_name = 't' ORDER BY name"
        )
        .await,
        [some(&["sqlite_autoindex_t_1"]), some(&["t_qty"])]
    );
    // The trigger was re-created, the view still works, the new check holds.
    run(c, "INSERT INTO t (id, code, qty) VALUES (3, 'c', 1.5)").await;
    assert_eq!(
        scalar(c, "SELECT msg FROM log ORDER BY rowid DESC LIMIT 1").await,
        Some("ins c".to_owned())
    );
    assert_eq!(
        query(c, "SELECT code, qty FROM t_view ORDER BY code").await,
        [some(&["a", "5"]), some(&["b", "7"]), some(&["c", "1.5"])]
    );
    assert!(
        exec_on(c, "INSERT INTO t (id, code, qty) VALUES (4, 'd', -1)", &[])
            .await
            .is_err()
    );
    // Enforcement and the rename setting are back; nothing is left over.
    assert_eq!(scalar(c, "PRAGMA foreign_keys").await, Some("1".to_owned()));
    assert_eq!(
        scalar(c, "PRAGMA legacy_alter_table").await,
        Some("0".to_owned())
    );
    let leftovers =
        format!("SELECT count(*) FROM sqlite_schema WHERE instr(name, '{REBUILD_SUFFIX}') > 0");
    assert_eq!(scalar(c, &leftovers).await, Some("0".to_owned()));
}

#[tokio::test]
async fn rebuild_rolls_back_on_a_foreign_key_violation() {
    let c = memory().await;
    let c = c.as_ref();
    run(
        c,
        "PRAGMA foreign_keys = ON;
         CREATE TABLE p (id INTEGER PRIMARY KEY);
         CREATE TABLE ch (id INTEGER PRIMARY KEY, p_id INTEGER);
         INSERT INTO p VALUES (1);
         INSERT INTO ch VALUES (1, 1), (2, 99);",
    )
    .await;
    let old = load_table(c, "ch").await;
    let mut new = old.clone();
    new.foreign_keys.push(ForeignKeyDef {
        name: Some("ch_p_fk".into()),
        columns: vec!["p_id".into()],
        ref_table: table_ref("p"),
        ref_columns: vec!["id".into()],
        on_delete: FkAction::Cascade,
        on_update: FkAction::NoAction,
        extras: Extras::new(),
    });
    let err = alter(c, &old, &new, &[]).await.unwrap_err();
    assert!(err.to_string().contains("foreign keys"), "{err}");
    // Everything rolled back; enforcement is on again.
    assert_same(&old, &load_table(c, "ch").await);
    assert_eq!(
        scalar(c, "SELECT count(*) FROM ch").await,
        Some("2".to_owned())
    );
    assert_eq!(scalar(c, "PRAGMA foreign_keys").await, Some("1".to_owned()));

    // With enforcement off the foreign key is added (unchecked) and enforcement stays off.
    run(c, "PRAGMA foreign_keys = OFF").await;
    alter(c, &old, &new, &[]).await.unwrap();
    assert_same(&new, &load_table(c, "ch").await);
    assert_eq!(scalar(c, "PRAGMA foreign_keys").await, Some("0".to_owned()));

    // Rebuilding the parent does not cascade into the children.
    run(c, "PRAGMA foreign_keys = ON; DELETE FROM ch WHERE id = 2").await;
    let parent = load_table(c, "p").await;
    let mut wider = parent.clone();
    wider.columns.push({
        let mut label = column("label", "TEXT", DataType::Text).not_null();
        label.default = Some("'-'".into());
        label
    });
    wider.checks.push(check("p_label_chk", "length(label) > 0"));
    alter(c, &parent, &wider, &[]).await.unwrap();
    assert_eq!(
        scalar(c, "SELECT count(*) FROM ch").await,
        Some("1".to_owned())
    );
    assert_same(&wider, &load_table(c, "p").await);
}

#[tokio::test]
async fn native_changes_and_renames_round_trip() {
    let c = memory().await;
    let c = c.as_ref();
    run(
        c,
        "CREATE TABLE r (id INTEGER PRIMARY KEY, a TEXT, b TEXT);
         CREATE INDEX r_a ON r (a);
         CREATE VIEW r_view AS SELECT id, b FROM r;
         INSERT INTO r VALUES (1, 'x', 'y');",
    )
    .await;
    let old = load_table(c, "r").await;
    let mut new = old.clone();
    new.obj.name = "r2".into();
    new.columns[2].name = "b \"2\"".into();
    new.columns.remove(1);
    new.indexes.clear();
    let mut extra = column("c", "INTEGER", DataType::INT64).not_null();
    extra.default = Some("0".into());
    new.columns.push(extra);
    new.indexes.push(index("r_c", &["c"]));
    let renames = [
        Rename::new(ObjectKind::Table, "r", "r2"),
        Rename::new(ObjectKind::Column, "b", "b \"2\""),
    ];
    let ctx = DdlContext::new(c.server_version())
        .with_table(&old)
        .with_renames(&renames);
    let script = ddl(ObjectKind::Table)
        .alter(
            &ObjectDef::Table(old.clone()),
            &ObjectDef::Table(new.clone()),
            &ctx,
        )
        .unwrap();
    assert!(!script.steps.iter().any(|s| s.sql.contains(REBUILD_SUFFIX)));
    apply(c, &script).await.unwrap();
    assert_same(&new, &load_table(c, "r2").await);
    assert!(table_gone(c, "r").await);
    // SQLite rewrote the view for the renames.
    assert_eq!(query(c, "SELECT * FROM r_view").await, [some(&["1", "y"])]);
    assert_eq!(
        query(c, "SELECT id, \"b \"\"2\"\"\", c FROM r2").await,
        [some(&["1", "y", "0"])]
    );
}

#[tokio::test]
async fn rebuild_keeps_the_autoincrement_counter_and_renamed_triggers_work() {
    let c = memory().await;
    let c = c.as_ref();
    run(
        c,
        "CREATE TABLE a (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT);
         CREATE TABLE seen (v TEXT);
         CREATE TRIGGER a_ins AFTER INSERT ON a BEGIN INSERT INTO seen VALUES (NEW.v); END;
         INSERT INTO a (v) VALUES ('x'), ('y'), ('z');
         DELETE FROM a WHERE id = 3;",
    )
    .await;
    let old = load_table(c, "a").await;
    let mut new = old.clone();
    new.obj.name = "a2".into();
    new.columns[1].name = "w".into();
    new.columns[1].native_type = "VARCHAR(10)".into();
    new.columns[1].data_type = DataType::VarChar { length: Some(10) };
    let renames = [
        Rename::new(ObjectKind::Table, "a", "a2"),
        Rename::new(ObjectKind::Column, "v", "w"),
    ];
    alter(c, &old, &new, &renames).await.unwrap();
    assert_same(&new, &load_table(c, "a2").await);
    run(c, "INSERT INTO a2 (w) VALUES ('q')").await;
    assert_eq!(
        scalar(c, "SELECT max(id) FROM a2").await,
        Some("4".to_owned())
    );
    assert_eq!(
        scalar(c, "SELECT v FROM seen ORDER BY rowid DESC LIMIT 1").await,
        Some("q".to_owned())
    );
}

#[tokio::test]
async fn rebuild_copies_values_without_casting_them() {
    let c = memory().await;
    let c = c.as_ref();
    run(
        c,
        "CREATE TABLE s (id INTEGER PRIMARY KEY, at TEXT, bin BLOB, n REAL);
         INSERT INTO s VALUES (1, '2024-05-01 10:00:00', X'00FF41', 1.5), (2, '2024-06-01', X'', 2.0);",
    )
    .await;
    let old = load_table(c, "s").await;
    let mut new = old.clone();
    new.columns[1].native_type = "DATETIME".into();
    new.columns[1].data_type = DataType::Timestamp { precision: None };
    new.columns[2].native_type = "TEXT".into();
    new.columns[2].data_type = DataType::Text;
    new.columns[3].native_type = "INTEGER".into();
    new.columns[3].data_type = DataType::INT64;
    alter(c, &old, &new, &[]).await.unwrap();
    assert_same(&new, &load_table(c, "s").await);
    // Affinity converts only where nothing is lost: the dates stay text, the blobs blobs, 1.5
    // stays a real, while 2.0 becomes an integer.
    assert_eq!(
        query(
            c,
            "SELECT at, typeof(at), hex(bin), typeof(bin), typeof(n) FROM s ORDER BY id"
        )
        .await,
        [
            some(&["2024-05-01 10:00:00", "text", "00FF41", "blob", "real"]),
            some(&["2024-06-01", "text", "", "blob", "integer"]),
        ]
    );

    // A STRICT table refuses a value it cannot convert, and the rebuild rolls back.
    run(
        c,
        "CREATE TABLE st (id INTEGER PRIMARY KEY, n TEXT) STRICT;
         INSERT INTO st VALUES (1, '12'), (2, 'x');",
    )
    .await;
    let old = load_table(c, "st").await;
    let mut new = old.clone();
    new.columns[1].native_type = "INTEGER".into();
    new.columns[1].data_type = DataType::INT64;
    assert!(alter(c, &old, &new, &[]).await.is_err());
    assert_same(&old, &load_table(c, "st").await);
    run(c, "DELETE FROM st WHERE id = 2").await;
    alter(c, &old, &new, &[]).await.unwrap();
    assert_eq!(
        query(c, "SELECT n, typeof(n) FROM st").await,
        [some(&["12", "integer"])]
    );
}

#[tokio::test]
async fn rebuild_with_renames_lets_sqlite_rewrite_triggers_and_views() {
    let c = memory().await;
    let c = c.as_ref();
    run(
        c,
        "PRAGMA foreign_keys = ON;
         CREATE TABLE log (qty INTEGER);
         CREATE TABLE t (id INTEGER PRIMARY KEY, qty INTEGER);
         CREATE TABLE ch (id INTEGER PRIMARY KEY, t_id INTEGER REFERENCES t (id));
         CREATE TRIGGER t_ins AFTER INSERT ON t BEGIN
             INSERT INTO log (qty) VALUES (NEW.qty);
         END;
         CREATE VIEW t_view AS SELECT id, qty FROM t;
         INSERT INTO t VALUES (1, 5);
         INSERT INTO ch VALUES (1, 1);",
    )
    .await;
    let old = load_table(c, "t").await;
    let mut new = old.clone();
    new.obj.name = "t2".into();
    new.columns[1].name = "amount".into();
    new.columns[1].native_type = "REAL".into();
    new.columns[1].data_type = DataType::FLOAT64;
    // The new check uses the new name; the rebuild writes it with the old one first.
    new.checks.push(check("t_amount_chk", "amount >= 0"));
    let renames = [
        Rename::new(ObjectKind::Table, "t", "t2"),
        Rename::new(ObjectKind::Column, "qty", "amount"),
    ];
    alter(c, &old, &new, &renames).await.unwrap();
    assert!(table_gone(c, "t").await);
    let fetched = load_table(c, "t2").await;
    let names: Vec<&str> = fetched
        .columns
        .iter()
        .map(|col| col.name.as_str())
        .collect();
    assert_eq!(names, ["id", "amount"]);
    assert_eq!(fetched.columns[1].native_type, "REAL");
    // The trigger still writes the column of `log` named qty, and reads NEW.amount.
    run(c, "INSERT INTO t2 (id, amount) VALUES (2, 7)").await;
    assert_eq!(
        query(c, "SELECT qty FROM log ORDER BY rowid").await,
        [some(&["5"]), some(&["7"])]
    );
    assert_eq!(
        query(c, "SELECT * FROM t_view ORDER BY id").await,
        [some(&["1", "5"]), some(&["2", "7"])]
    );
    assert!(
        exec_on(c, "INSERT INTO t2 (id, amount) VALUES (3, -1)", &[])
            .await
            .is_err()
    );
    // The child's foreign key follows the table rename.
    assert!(
        exec_on(c, "INSERT INTO ch VALUES (2, 99)", &[])
            .await
            .is_err()
    );
    run(c, "INSERT INTO ch VALUES (3, 2)").await;
    assert_eq!(scalar(c, "PRAGMA foreign_keys").await, Some("1".to_owned()));
    assert_eq!(
        scalar(c, "PRAGMA legacy_alter_table").await,
        Some("0".to_owned())
    );
}

#[tokio::test]
async fn rebuild_refuses_what_it_cannot_re_create() {
    let c = memory().await;
    let c = c.as_ref();
    run(
        c,
        "PRAGMA foreign_keys = ON;
         CREATE TABLE t (id INTEGER PRIMARY KEY, note TEXT);
         CREATE TABLE seen (id INTEGER);
         CREATE TRIGGER t_ins AFTER INSERT ON t BEGIN INSERT INTO seen VALUES (NEW.id); END;
         INSERT INTO t VALUES (1, 'a');",
    )
    .await;
    // A caller that passes no triggers: the script stops before dropping the table.
    let old = load_table(c, "t").await;
    let mut new = old.clone();
    new.columns[1].nullable = false;
    let version = c.server_version().clone();
    let script = ddl(ObjectKind::Table)
        .alter(
            &ObjectDef::Table(old.clone()),
            &ObjectDef::Table(new.clone()),
            &DdlContext::new(&version).with_table(&old),
        )
        .unwrap();
    let err = apply(c, &script).await.unwrap_err();
    assert!(err.to_string().contains("trigger"), "{err}");
    assert_eq!(triggers_of(c, "t").await.len(), 1);
    assert_same(&old, &load_table(c, "t").await);
    // With the triggers it works.
    alter(c, &old, &new, &[]).await.unwrap();
    assert_eq!(triggers_of(c, "t").await.len(), 1);

    // Clauses the model cannot hold are noted and a rebuild is refused.
    run(
        c,
        "CREATE TABLE oc (id INTEGER PRIMARY KEY, a TEXT NOT NULL ON CONFLICT REPLACE DEFAULT 'x',
                          b TEXT, UNIQUE (b COLLATE NOCASE));
         CREATE TABLE ix (id INTEGER PRIMARY KEY, b TEXT);
         CREATE INDEX ix_b ON ix (b COLLATE NOCASE);",
    )
    .await;
    for name in ["oc", "ix"] {
        let old = load_table(c, name).await;
        let mut new = old.clone();
        new.columns[1].native_type = "VARCHAR(5)".into();
        new.columns[1].data_type = DataType::VarChar { length: Some(5) };
        let err = alter(c, &old, &new, &[]).await.unwrap_err();
        assert_eq!(err.category(), ErrorCategory::Unsupported, "{err}");
        // The note is not a difference.
        assert_same(&old, &load_table(c, name).await);
    }

    // A parent-side change that breaks a child's foreign key rolls back.
    run(
        c,
        "CREATE TABLE p (id INTEGER PRIMARY KEY, k TEXT UNIQUE);
         CREATE TABLE pc (k TEXT REFERENCES p (k));
         INSERT INTO p VALUES (1, 'a');
         INSERT INTO pc VALUES ('a');",
    )
    .await;
    let old = load_table(c, "p").await;
    let mut new = old.clone();
    new.indexes.retain(|i| !i.is_constraint);
    assert!(alter(c, &old, &new, &[]).await.is_err());
    assert_same(&old, &load_table(c, "p").await);
    assert_eq!(scalar(c, "PRAGMA foreign_keys").await, Some("1".to_owned()));
}

// --- live: the fixture ---------------------------------------------------------------------

#[tokio::test]
#[ignore = "live: conformance job (fixture file)"]
async fn live_round_trip_table() {
    let (_dir, c) = fixture().await;
    let c = c.as_ref();
    run(c, "PRAGMA foreign_keys = ON").await;
    let name = format!("m3_rt_table_{}", token());
    let mut t = TableDef::new(table_ref(&name));
    t.columns.push(id_column());
    let mut order = column(
        "order",
        "VARCHAR(20)",
        DataType::VarChar { length: Some(20) },
    )
    .not_null();
    order.default = Some(AnsiQuoting.quote_str("it's"));
    t.columns.push(order);
    let mut quoted = column("a\"b", "TEXT", DataType::Text);
    quoted.default = Some(AnsiQuoting.quote_str("C:\\path ü"));
    quoted.collation = Some("NOCASE".into());
    t.columns.push(quoted);
    t.columns.push(column("名前", "REAL", DataType::FLOAT64));
    t.columns.push(column("parent", "INTEGER", DataType::INT64));
    t.primary_key = Some(pk(&["id"]));
    let mut by_name = index(&format!("{name}_idx"), &["名前"]);
    by_name.columns[0].descending = true;
    t.indexes.push(by_name);
    t.checks
        .push(check(&format!("{name}_chk"), "length(\"order\") > 0"));
    t.foreign_keys.push(ForeignKeyDef {
        name: Some(format!("{name}_fk")),
        columns: vec!["parent".into()],
        ref_table: table_ref("m2_parent"),
        ref_columns: vec!["id".into()],
        on_delete: FkAction::Cascade,
        on_update: FkAction::NoAction,
        extras: Extras::new(),
    });
    let ctx_version = c.server_version().clone();
    let script = ddl(ObjectKind::Table)
        .create(&ObjectDef::Table(t.clone()), &DdlContext::new(&ctx_version))
        .unwrap();
    apply(c, &script).await.unwrap();
    let created = load_table(c, &name).await;
    assert_same(&t, &created);

    // Rename a column, add one and change a type (a rebuild after the rename).
    let mut changed = created.clone();
    changed.columns[2].name = "a\"b2".into();
    changed.columns[3].native_type = "NUMERIC".into();
    changed.columns[3].data_type = DataType::Decimal {
        precision: None,
        scale: None,
    };
    changed
        .columns
        .push(column("extra", "TEXT", DataType::Text));
    let renames = [Rename::new(ObjectKind::Column, "a\"b", "a\"b2")];
    alter(c, &created, &changed, &renames).await.unwrap();
    assert_same(&changed, &load_table(c, &name).await);

    let drop = ddl(ObjectKind::Table)
        .drop_object(&table_ref(&name), &DdlContext::new(&ctx_version))
        .unwrap();
    apply(c, &drop).await.unwrap();
    assert!(table_gone(c, &name).await);
}

#[tokio::test]
#[ignore = "live: conformance job (fixture file)"]
async fn live_round_trip_column() {
    let (_dir, c) = fixture().await;
    let c = c.as_ref();
    let name = format!("m3_rt_column_{}", token());
    let tref = table_ref(&name);
    run(
        c,
        &format!("CREATE TABLE \"{name}\" (id INTEGER PRIMARY KEY, a TEXT)"),
    )
    .await;
    run(
        c,
        &format!("INSERT INTO \"{name}\" (id, a) VALUES (1, 'one')"),
    )
    .await;
    let version = c.server_version().clone();
    let generator = ddl(ObjectKind::Column);

    let cur = load_table(c, &name).await;
    let mut col = column(
        "名 \"q\"",
        "VARCHAR(10)",
        DataType::VarChar { length: Some(10) },
    )
    .not_null();
    col.default = Some(AnsiQuoting.quote_str("x'y"));
    let script = generator
        .create(
            &ObjectDef::Column {
                table: tref.clone(),
                column: col.clone(),
            },
            &DdlContext::new(&version).with_table(&cur),
        )
        .unwrap();
    apply(c, &script).await.unwrap();
    let mut expected = cur.clone();
    expected.columns.push(col.clone());
    let fetched = load_table(c, &name).await;
    assert_same(&expected, &fetched);
    let loaded = fetcher(ObjectKind::Column)
        .load(
            c,
            &tref.child(ObjectKind::Column, col.name.as_str()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let column_diff = registry()
        .comparator(Engine::Sqlite, ObjectKind::Column)
        .unwrap()
        .diff(
            &ObjectDef::Column {
                table: tref.clone(),
                column: col.clone(),
            },
            &loaded,
        );
    assert_eq!(column_diff, Vec::<PropertyDiff>::new());
    assert_eq!(
        scalar(c, &format!("SELECT \"名 \"\"q\"\"\" FROM \"{name}\"")).await,
        Some("x'y".to_owned())
    );

    // Change its type and nullability: a rebuild.
    let mut wider = col.clone();
    wider.native_type = "TEXT".into();
    wider.data_type = DataType::Text;
    wider.nullable = true;
    let script = generator
        .alter(
            &ObjectDef::Column {
                table: tref.clone(),
                column: col.clone(),
            },
            &ObjectDef::Column {
                table: tref.clone(),
                column: wider.clone(),
            },
            &DdlContext::new(&version).with_table(&fetched),
        )
        .unwrap();
    assert!(script.is_destructive());
    apply(c, &script).await.unwrap();
    let mut expected = fetched.clone();
    expected.columns[2].clone_from(&wider);
    let fetched = load_table(c, &name).await;
    assert_same(&expected, &fetched);

    let script = generator
        .drop_object(
            &tref.child(ObjectKind::Column, wider.name.as_str()),
            &DdlContext::new(&version).with_table(&fetched),
        )
        .unwrap();
    apply(c, &script).await.unwrap();
    assert_same(&cur, &load_table(c, &name).await);
    assert_eq!(
        query(c, &format!("SELECT * FROM \"{name}\"")).await,
        [some(&["1", "one"])]
    );
}

#[tokio::test]
#[ignore = "live: conformance job (fixture file)"]
async fn live_round_trip_index() {
    let (_dir, c) = fixture().await;
    let c = c.as_ref();
    let name = format!("m3_rt_index_{}", token());
    let tref = table_ref(&name);
    run(
        c,
        &format!("CREATE TABLE \"{name}\" (id INTEGER PRIMARY KEY, a TEXT, b INTEGER)"),
    )
    .await;
    let version = c.server_version().clone();
    let generator = ddl(ObjectKind::Index);
    let index_cmp = registry()
        .comparator(Engine::Sqlite, ObjectKind::Index)
        .unwrap();

    let mut idx = index(&format!("{name}_ab"), &["a", "b"]);
    idx.columns[1].descending = true;
    idx.predicate = Some("b > 0".into());
    let def = ObjectDef::Index {
        table: tref.clone(),
        index: idx.clone(),
    };
    apply(
        c,
        &generator.create(&def, &DdlContext::new(&version)).unwrap(),
    )
    .await
    .unwrap();
    let iref = tref.child(ObjectKind::Index, idx.name.as_str());
    let loaded = fetcher(ObjectKind::Index)
        .load(c, &iref, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(index_cmp.diff(&def, &loaded), Vec::<PropertyDiff>::new());

    let mut changed = idx.clone();
    changed.unique = true;
    changed.predicate = None;
    changed.columns[0] = IndexColumn {
        name: "lower(a)".into(),
        is_expression: true,
        descending: false,
        length: None,
    };
    let new_def = ObjectDef::Index {
        table: tref.clone(),
        index: changed,
    };
    apply(
        c,
        &generator
            .alter(&def, &new_def, &DdlContext::new(&version))
            .unwrap(),
    )
    .await
    .unwrap();
    let loaded = fetcher(ObjectKind::Index)
        .load(c, &iref, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        index_cmp.diff(&new_def, &loaded),
        Vec::<PropertyDiff>::new()
    );

    apply(
        c,
        &generator
            .drop_object(&iref, &DdlContext::new(&version))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(load_table(c, &name).await.indexes, Vec::<IndexDef>::new());
}

#[tokio::test]
#[ignore = "live: conformance job (fixture file)"]
async fn live_round_trip_foreign_key() {
    let (_dir, c) = fixture().await;
    let c = c.as_ref();
    let tok = token();
    let name = format!("m3_rt_foreign_key_{tok}");
    let parent = format!("m3_rt_fk_{tok}_ref");
    let tref = table_ref(&name);
    run(
        c,
        &format!(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE \"{parent}\" (id INTEGER PRIMARY KEY, code TEXT NOT NULL UNIQUE);
             CREATE TABLE \"{name}\" (id INTEGER PRIMARY KEY, ref_id INTEGER, ref_code TEXT);
             INSERT INTO \"{parent}\" VALUES (1, 'one'), (2, 'two');
             INSERT INTO \"{name}\" VALUES (1, 1, 'one'), (2, 2, 'two');"
        ),
    )
    .await;
    let version = c.server_version().clone();
    let generator = ddl(ObjectKind::ForeignKey);
    let cur = load_table(c, &name).await;
    let fk = ForeignKeyDef {
        name: Some(format!("{name}_fk")),
        columns: vec!["ref_id".into()],
        ref_table: table_ref(&parent),
        ref_columns: vec!["id".into()],
        on_delete: FkAction::Cascade,
        on_update: FkAction::NoAction,
        extras: Extras::new(),
    };
    let def = ObjectDef::ForeignKey {
        table: tref.clone(),
        foreign_key: fk.clone(),
    };
    apply(
        c,
        &generator
            .create(&def, &DdlContext::new(&version).with_table(&cur))
            .unwrap(),
    )
    .await
    .unwrap();
    let mut expected = cur.clone();
    expected.foreign_keys.push(fk.clone());
    let fetched = load_table(c, &name).await;
    assert_same(&expected, &fetched);

    let mut changed = fk.clone();
    changed.columns = vec!["ref_code".into()];
    changed.ref_columns = vec!["code".into()];
    changed.on_delete = FkAction::SetNull;
    changed.on_update = FkAction::Cascade;
    let new_def = ObjectDef::ForeignKey {
        table: tref.clone(),
        foreign_key: changed.clone(),
    };
    apply(
        c,
        &generator
            .alter(
                &def,
                &new_def,
                &DdlContext::new(&version).with_table(&fetched),
            )
            .unwrap(),
    )
    .await
    .unwrap();
    let mut expected = cur.clone();
    expected.foreign_keys.push(changed.clone());
    let fetched = load_table(c, &name).await;
    assert_same(&expected, &fetched);
    // The foreign key is enforced: updating the parent code cascades.
    run(
        c,
        &format!("UPDATE \"{parent}\" SET code = 'uno' WHERE id = 1"),
    )
    .await;
    assert_eq!(
        scalar(c, &format!("SELECT ref_code FROM \"{name}\" WHERE id = 1")).await,
        Some("uno".to_owned())
    );

    let fk_ref = tref.child(ObjectKind::ForeignKey, format!("{name}_fk"));
    apply(
        c,
        &generator
            .drop_object(&fk_ref, &DdlContext::new(&version).with_table(&fetched))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_same(&cur, &load_table(c, &name).await);
    assert_eq!(scalar(c, "PRAGMA foreign_keys").await, Some("1".to_owned()));
}

#[tokio::test]
#[ignore = "live: conformance job (fixture file)"]
async fn live_round_trip_check() {
    let (_dir, c) = fixture().await;
    let c = c.as_ref();
    let name = format!("m3_rt_check_{}", token());
    let tref = table_ref(&name);
    run(
        c,
        &format!(
            "CREATE TABLE \"{name}\" (id INTEGER PRIMARY KEY, qty INTEGER);
             INSERT INTO \"{name}\" VALUES (1, 5);"
        ),
    )
    .await;
    let version = c.server_version().clone();
    let generator = ddl(ObjectKind::Check);
    let cur = load_table(c, &name).await;
    let chk = check(&format!("{name}_chk"), "qty >= 0 AND note_free = 'it''s'");
    // A check that does not compile is refused by SQLite and nothing changes.
    let bad = ObjectDef::Check {
        table: tref.clone(),
        check: chk,
    };
    let script = generator
        .create(&bad, &DdlContext::new(&version).with_table(&cur))
        .unwrap();
    assert!(apply(c, &script).await.is_err());
    assert_same(&cur, &load_table(c, &name).await);

    let chk = check(&format!("{name}_chk"), "qty >= 0 AND 'it''s' <> ''");
    let def = ObjectDef::Check {
        table: tref.clone(),
        check: chk.clone(),
    };
    apply(
        c,
        &generator
            .create(&def, &DdlContext::new(&version).with_table(&cur))
            .unwrap(),
    )
    .await
    .unwrap();
    let mut expected = cur.clone();
    expected.checks.push(chk.clone());
    let fetched = load_table(c, &name).await;
    assert_same(&expected, &fetched);
    let loaded = fetcher(ObjectKind::Check)
        .load(
            c,
            &tref.child(ObjectKind::Check, format!("{name}_chk")),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        registry()
            .comparator(Engine::Sqlite, ObjectKind::Check)
            .unwrap()
            .diff(&def, &loaded),
        Vec::<PropertyDiff>::new()
    );

    let mut narrower = chk.clone();
    narrower.expression = "qty BETWEEN 0 AND 1000".into();
    let new_def = ObjectDef::Check {
        table: tref.clone(),
        check: narrower.clone(),
    };
    apply(
        c,
        &generator
            .alter(
                &def,
                &new_def,
                &DdlContext::new(&version).with_table(&fetched),
            )
            .unwrap(),
    )
    .await
    .unwrap();
    let mut expected = cur.clone();
    expected.checks.push(narrower);
    let fetched = load_table(c, &name).await;
    assert_same(&expected, &fetched);
    assert!(
        exec_on(c, &format!("UPDATE \"{name}\" SET qty = 5000"), &[])
            .await
            .is_err()
    );

    let script = generator
        .drop_object(
            &tref.child(ObjectKind::Check, format!("{name}_chk")),
            &DdlContext::new(&version).with_table(&fetched),
        )
        .unwrap();
    apply(c, &script).await.unwrap();
    assert_same(&cur, &load_table(c, &name).await);
    assert_eq!(
        query(c, &format!("SELECT * FROM \"{name}\"")).await,
        [some(&["1", "5"])]
    );
}

#[tokio::test]
#[ignore = "live: conformance job (fixture file)"]
async fn live_rebuild_keeps_the_fixture_view_trigger_and_foreign_keys() {
    let (_dir, c) = fixture().await;
    let c = c.as_ref();
    run(
        c,
        "PRAGMA foreign_keys = ON;
         INSERT INTO m2_parent (id, code) VALUES (1, 'p1'), (2, 'p2');
         INSERT INTO m2_child (id, parent_id, qty) VALUES (1, 1, 3), (2, 2, 4);",
    )
    .await;
    let old = load_table(c, "m2_child").await;
    let mut new = old.clone();
    let qty = new
        .columns
        .iter_mut()
        .find(|col| col.name == "qty")
        .unwrap();
    qty.default = Some("2".into());
    alter(c, &old, &new, &[]).await.unwrap();
    assert_same(&new, &load_table(c, "m2_child").await);

    // The view on the rebuilt table still works.
    assert_eq!(
        query(c, "SELECT id, qty, code FROM m2_view ORDER BY id").await,
        [some(&["1", "3", "p1"]), some(&["2", "4", "p2"])]
    );
    // The trigger was re-created.
    let err = exec_on(
        c,
        "INSERT INTO m2_child (id, parent_id, qty) VALUES (3, 1, -1)",
        &[],
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("must not be negative"), "{err}");
    // The foreign key is enforced and the new default applies.
    assert!(
        exec_on(
            c,
            "INSERT INTO m2_child (id, parent_id) VALUES (4, 99)",
            &[]
        )
        .await
        .is_err()
    );
    run(c, "INSERT INTO m2_child (id, parent_id) VALUES (5, 1)").await;
    assert_eq!(
        scalar(c, "SELECT qty FROM m2_child WHERE id = 5").await,
        Some("2".to_owned())
    );
    assert_eq!(triggers_of(c, "m2_child").await.len(), 1);

    // Rebuilding the parent keeps the children (the old parent's drop does not cascade).
    let parent = load_table(c, "m2_parent").await;
    let mut wider = parent.clone();
    let note = wider
        .columns
        .iter_mut()
        .find(|col| col.name == "note")
        .unwrap();
    note.native_type = "VARCHAR(200)".into();
    note.data_type = DataType::VarChar { length: Some(200) };
    alter(c, &parent, &wider, &[]).await.unwrap();
    assert_same(&wider, &load_table(c, "m2_parent").await);
    assert_eq!(
        scalar(c, "SELECT count(*) FROM m2_child").await,
        Some("3".to_owned())
    );
    assert_eq!(scalar(c, "PRAGMA foreign_keys").await, Some("1".to_owned()));
    assert_eq!(
        query(c, "SELECT count(*) FROM m2_view").await,
        [some(&["3"])]
    );
}

// --- live: grid writes ---------------------------------------------------------------------

/// Runs one plan inside `tx`: the affected-row count (returned rows when `returning`, else the
/// summed `RowsAffected`) and the row read back.
async fn run_plan(tx: &dyn Tx, plan: &DmlPlan) -> Result<(usize, Rows)> {
    let got = exec_tx(tx, &plan.write.sql, &plan.write.params).await?;
    if plan.returning {
        let rows = rows_of(&got);
        return Ok((rows.len(), rows));
    }
    let affected = usize::try_from(got.rows_affected.iter().sum::<u64>()).unwrap();
    let rows = match &plan.read_back {
        Some(back) => rows_of(&exec_tx(tx, &back.sql, &back.params).await?),
        None => Vec::new(),
    };
    Ok((affected, rows))
}

struct Target {
    table: TableDef,
    key: RowKey,
    read_back: Vec<String>,
}

impl Target {
    async fn load(c: &dyn Connection, name: &str, key: &[&str]) -> Self {
        let table = load_table(c, name).await;
        let read_back = table.columns.iter().map(|col| col.name.clone()).collect();
        let kind = if table.primary_key.is_some() {
            KeyKind::Primary
        } else {
            KeyKind::Unique
        };
        let key = RowKey::new(
            table.obj.clone(),
            kind,
            key.iter().map(|k| (*k).to_owned()).collect(),
        );
        Self {
            table,
            key,
            read_back,
        }
    }

    fn plan(&self, c: &dyn Connection, edit: &RowEdit) -> Result<DmlPlan> {
        self.plan_for(c.server_version(), edit)
    }

    fn plan_for(&self, version: &oxbase_types::ServerVersion, edit: &RowEdit) -> Result<DmlPlan> {
        writer().plan(
            edit,
            &DmlTarget {
                table: &self.table,
                key: &self.key,
                version,
                read_back: &self.read_back,
            },
        )
    }
}

/// Applies `edits` in one transaction the way the edit service does: every write must affect
/// exactly one row, else everything rolls back. Returns the rows read back.
async fn apply_edits(c: &dyn Connection, target: &Target, edits: &[RowEdit]) -> Result<Vec<Rows>> {
    let mut plans = Vec::new();
    for edit in edits {
        plans.push(target.plan(c, edit)?);
    }
    let tx = c.begin().await?;
    let mut out = Vec::new();
    for plan in &plans {
        let outcome = run_plan(tx.as_ref(), plan).await;
        match outcome {
            Ok((1, rows)) => out.push(rows),
            Ok((n, _)) => {
                tx.rollback().await?;
                return Err(Error::invalid_input(format!(
                    "conflict: {n} rows for {}",
                    plan.write.sql
                )));
            }
            Err(e) => {
                tx.rollback().await?;
                return Err(e);
            }
        }
    }
    tx.commit().await?;
    Ok(out)
}

fn col_index(target: &Target, name: &str) -> usize {
    target.read_back.iter().position(|c| c == name).unwrap()
}

#[tokio::test]
#[ignore = "live: conformance job (fixture file)"]
async fn live_dml_writes_with_bound_values() {
    let (dir, c) = fixture().await;
    let c = c.as_ref();
    let tok = token();
    let pk_t = Target::load(c, "m3_edit_pk", &["id"]).await;
    let at =
        NaiveDateTime::parse_from_str("2025-06-07 08:09:10.5", "%Y-%m-%d %H:%M:%S%.f").unwrap();
    let insert = |code: &str, name: Value| RowEdit::Insert {
        values: vec![
            ColumnValue::new("code", format!("t{tok}-{code}")),
            ColumnValue::new("name", name),
            ColumnValue::new("amount", Value::Decimal("1.25".into())),
            ColumnValue::new("flag", true),
            ColumnValue::new("born", NaiveDate::from_ymd_opt(2025, 6, 7).unwrap()),
            ColumnValue::new("at", at),
            ColumnValue::new("doc", r#"{"k": [1, "two"]}"#),
            ColumnValue::new("data", Value::Bytes(vec![0, 0xff, b'\''])),
        ],
    };
    // NULL and the empty string stay apart; the generated key and column come back.
    let back = apply_edits(
        c,
        &pk_t,
        &[
            insert("1", Value::Text(String::new())),
            insert("2", Value::Null),
        ],
    )
    .await
    .unwrap();
    let (id, name, total) = (
        col_index(&pk_t, "id"),
        col_index(&pk_t, "name"),
        col_index(&pk_t, "total"),
    );
    let first = &back[0][0];
    let second = &back[1][0];
    let id1: i64 = first[id].as_deref().unwrap().parse().unwrap();
    let id2: i64 = second[id].as_deref().unwrap().parse().unwrap();
    assert!(id1 > 3 && id2 == id1 + 1, "{id1} {id2}");
    assert_eq!(first[name].as_deref(), Some(""));
    assert_eq!(second[name], None);
    assert_eq!(first[total].as_deref(), Some("2.5"));
    assert_eq!(
        query(
            c,
            &format!(
                "SELECT name IS NULL, name = '', data FROM m3_edit_pk WHERE id IN ({id1}, {id2}) \
                 ORDER BY id"
            )
        )
        .await,
        [
            vec![
                Some("0".to_owned()),
                Some("1".to_owned()),
                Some("00ff27".to_owned())
            ],
            vec![Some("1".to_owned()), None, Some("00ff27".to_owned())],
        ]
    );

    // Update by key, reading the row back; a key change reads back by the new key.
    let update = RowEdit::Update {
        key: vec![Value::Int(id1)],
        changes: vec![
            ColumnValue::new("name", "x'; DROP TABLE m3_edit_pk; --"),
            ColumnValue::new("amount", Value::Decimal("10".into())),
        ],
    };
    let back = apply_edits(c, &pk_t, &[update]).await.unwrap();
    assert_eq!(
        back[0][0][name].as_deref(),
        Some("x'; DROP TABLE m3_edit_pk; --")
    );
    assert_eq!(back[0][0][total].as_deref(), Some("20"));

    // Optimistic concurrency: another session deletes one row and re-keys the other.
    let other = connect(&dir.path().join("oxbase_test.sqlite")).await;
    run(
        other.as_ref(),
        &format!("DELETE FROM m3_edit_pk WHERE id = {id2}"),
    )
    .await;
    let touch = |key: i64| RowEdit::Update {
        key: vec![Value::Int(key)],
        changes: vec![ColumnValue::new("name", "touched")],
    };
    let err = apply_edits(c, &pk_t, &[touch(id1), touch(id2)])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("conflict: 0 rows"), "{err}");
    // The first update was rolled back with it.
    assert_eq!(
        scalar(c, &format!("SELECT name FROM m3_edit_pk WHERE id = {id1}")).await,
        Some("x'; DROP TABLE m3_edit_pk; --".to_owned())
    );
    run(
        other.as_ref(),
        &format!("UPDATE m3_edit_pk SET id = id + 1000 WHERE id = {id1}"),
    )
    .await;
    assert!(apply_edits(c, &pk_t, &[touch(id1)]).await.is_err());

    // Deletes count affected rows; a second delete finds nothing.
    let delete = RowEdit::Delete {
        key: vec![Value::Int(id1 + 1000)],
    };
    apply_edits(c, &pk_t, std::slice::from_ref(&delete))
        .await
        .unwrap();
    assert!(apply_edits(c, &pk_t, &[delete]).await.is_err());

    // Generated columns are never written; nothing reaches the database.
    let generated = RowEdit::Insert {
        values: vec![
            ColumnValue::new("code", format!("t{tok}-g")),
            ColumnValue::new("total", 1),
        ],
    };
    let err = pk_t.plan(c, &generated).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);

    // Seed rows are untouched.
    assert_eq!(
        query(
            c,
            "SELECT id, code FROM m3_edit_pk WHERE id <= 3 ORDER BY id"
        )
        .await,
        [some(&["1", "a"]), some(&["2", "b"]), some(&["3", "c"])]
    );
    assert_eq!(
        scalar(
            c,
            &format!("SELECT count(*) FROM m3_edit_pk WHERE code LIKE 't{tok}-%'")
        )
        .await,
        Some("0".to_owned())
    );
}

#[tokio::test]
#[ignore = "live: conformance job (fixture file)"]
async fn live_dml_unique_key_rowid_key_defaults_and_old_libraries() {
    let (_dir, c) = fixture().await;
    let c = c.as_ref();
    let tok = token();

    // m3_edit_uq: edited by its NOT NULL unique code.
    let uq = Target::load(c, "m3_edit_uq", &["code"]).await;
    let code = format!("t{tok}-u");
    let back = apply_edits(
        c,
        &uq,
        &[
            RowEdit::Insert {
                values: vec![
                    ColumnValue::new("code", code.clone()),
                    ColumnValue::new("val", 7),
                ],
            },
            RowEdit::Update {
                key: vec![Value::Text(code.clone())],
                changes: vec![ColumnValue::new("val", 8)],
            },
        ],
    )
    .await
    .unwrap();
    assert_eq!(back[1][0], [Some(code.clone()), Some("8".to_owned())]);
    apply_edits(
        c,
        &uq,
        &[RowEdit::Delete {
            key: vec![Value::Text(code)],
        }],
    )
    .await
    .unwrap();

    // m3_edit_nokey: a row of defaults, then found again by its rowid.
    let mut nokey = Target::load(c, "m3_edit_nokey", &["rowid"]).await;
    nokey.read_back.insert(0, "rowid".to_owned());
    let back = apply_edits(c, &nokey, &[RowEdit::Insert { values: vec![] }])
        .await
        .unwrap();
    let row = &back[0][0];
    assert_eq!(row[1..], [None::<String>, None, None]);
    let rowid: i64 = row[0].as_deref().unwrap().parse().unwrap();
    apply_edits(
        c,
        &nokey,
        &[RowEdit::Delete {
            key: vec![Value::Int(rowid)],
        }],
    )
    .await
    .unwrap();
    assert_eq!(
        query(c, "SELECT a, b, u FROM m3_edit_nokey").await,
        [some(&["1", "one", "1"])]
    );

    // Libraries before 3.35 read the row back with a SELECT (last_insert_rowid()).
    let pk_t = Target::load(c, "m3_edit_pk", &["id"]).await;
    let old: oxbase_types::ServerVersion = "3.34.1".parse().unwrap();
    let plan = pk_t
        .plan_for(
            &old,
            &RowEdit::Insert {
                values: vec![
                    ColumnValue::new("code", format!("t{tok}-old")),
                    ColumnValue::new("amount", Value::Decimal("3".into())),
                ],
            },
        )
        .unwrap();
    assert!(!plan.returning && plan.read_back.is_some());
    let tx = c.begin().await.unwrap();
    let (affected, rows) = run_plan(tx.as_ref(), &plan).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(affected, 1);
    let total = col_index(&pk_t, "total");
    let id = col_index(&pk_t, "id");
    assert_eq!(rows[0][total].as_deref(), Some("6"));
    let new_id: i64 = rows[0][id].as_deref().unwrap().parse().unwrap();
    let update = pk_t
        .plan_for(
            &old,
            &RowEdit::Update {
                key: vec![Value::Int(new_id)],
                changes: vec![ColumnValue::new("id", new_id + 500)],
            },
        )
        .unwrap();
    let tx = c.begin().await.unwrap();
    let (affected, rows) = run_plan(tx.as_ref(), &update).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(affected, 1);
    assert_eq!(rows[0][id], Some((new_id + 500).to_string()));
    run(
        c,
        &format!("DELETE FROM m3_edit_pk WHERE id = {}", new_id + 500),
    )
    .await;
}
