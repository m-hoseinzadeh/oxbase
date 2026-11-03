//! Data compare and data sync end to end on real SQLite files (no Docker): the `[m8]` data
//! pairs built in two temporary databases, compared in chunks (several chunk sizes), the
//! differing rows read back from the spill store as bound statements, applied to the target in
//! one transaction, and compared again (in sync). `docs/m8-contracts.md` §6, §12.

use std::sync::Mutex;

use oxbase_dialect::{DialectRegistry, inline_params};
use oxbase_diff::data::{CompareJob, compare_table, data_statements, row_key};
use oxbase_diff::{
    DataCompareOptions, DiffStore, KeyChoice, RowCounts, RowOp, TableCompareStatus, TableDataDiff,
};
use oxbase_driver::{CancellationToken, ConnectParams, Connection, Driver, collect};
use oxbase_sqlite::SqliteDriver;
use oxbase_types::{Engine, ObjectDef, ObjectKind, ObjectRef, ServerVersion, TableDef};

const SCHEMA: &str = "
CREATE TABLE m8_data (id BIGINT NOT NULL PRIMARY KEY, name VARCHAR(100) NOT NULL,
  amount TEXT, payload BLOB, happened_at TEXT, note TEXT);
CREATE TABLE m8_big (id BIGINT NOT NULL PRIMARY KEY, val INT, label VARCHAR(20));
CREATE TABLE m8_composite (tenant INT NOT NULL, code VARCHAR(20) NOT NULL, qty INT,
  PRIMARY KEY (tenant, code));
CREATE TABLE m8_nokey (a INT, b TEXT);
";

/// `m8_data` rows 1..=`upto` (unicode names, a big decimal as text, bytes, an offset
/// timestamp), `m8_big` 1..=100000, `m8_composite` 6 rows, `m8_nokey` 1 row.
fn seed(upto: u32) -> String {
    format!(
        "
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {upto})
INSERT INTO m8_data SELECT i,
  CASE i % 3 WHEN 0 THEN 'grüße ' || i WHEN 1 THEN '日本 ' || i ELSE '😀 ' || i END,
  CASE WHEN i % 4 = 0 THEN '-1234567890123456789012345678.0123456789'
       ELSE '1234567890123456789012345678.012345678' || (i % 10) END,
  CASE WHEN i % 2 = 0 THEN X'00ff10' ELSE NULL END,
  '2026-03-01T10:00:00.123456+05:30',
  CASE WHEN i = 10 THEN 'y' ELSE NULL END
FROM n;
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 100000)
INSERT INTO m8_big SELECT i, i % 1000, 'row-' || i FROM n;
INSERT INTO m8_composite VALUES (1, 'a', 1), (1, 'b', 2), (2, 'a', 3), (2, 'b', 4), (3, 'a', 5),
  (3, 'b', 6);
INSERT INTO m8_nokey VALUES (1, 'x');
"
    )
}

/// The designed target differences (`tests/fixtures.toml` `[m8]` data pairs).
const TARGET_CHANGES: &str = "
UPDATE m8_data SET name = 'changed' WHERE id = 3;
UPDATE m8_data SET amount = '-1.5' WHERE id = 5;
UPDATE m8_data SET happened_at = '2026-03-01T10:00:01.123456+05:30' WHERE id = 7;
UPDATE m8_data SET note = 'x' WHERE id = 9;
UPDATE m8_data SET note = NULL WHERE id = 10;
INSERT INTO m8_data VALUES (21, 'extra', '0', NULL, NULL, NULL), (22, 'extra 2', '1', X'', '', '');
UPDATE m8_big SET val = 9999 WHERE id = 500;
DELETE FROM m8_big WHERE id = 50000;
INSERT INTO m8_big VALUES (100001, 1, 'row-100001');
UPDATE m8_composite SET qty = 40 WHERE tenant = 2 AND code = 'b';
";

const TABLES: [&str; 4] = ["m8_data", "m8_big", "m8_composite", "m8_nokey"];

async fn open(path: &std::path::Path) -> Box<dyn Connection> {
    SqliteDriver
        .connect(&ConnectParams::sqlite(path))
        .await
        .unwrap()
}

async fn run(c: &dyn Connection, sql: &str) {
    collect(c.execute(sql, &[], CancellationToken::new()).await.unwrap())
        .await
        .unwrap_or_else(|e| panic!("{e}: {sql}"));
}

fn registry() -> DialectRegistry {
    let mut reg = DialectRegistry::new();
    oxbase_sqlite::dialect::register(&mut reg);
    reg
}

fn table_ref(name: &str) -> ObjectRef {
    ObjectRef::table(Some("main"), None, name)
}

async fn load(reg: &DialectRegistry, c: &dyn Connection, name: &str) -> TableDef {
    let fetcher = reg.fetcher(Engine::Sqlite, ObjectKind::Table).unwrap();
    match fetcher
        .load(c, &table_ref(name), CancellationToken::new())
        .await
        .unwrap()
    {
        ObjectDef::Table(t) => t,
        other => panic!("not a table: {other:?}"),
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    reg: DialectRegistry,
    source: Box<dyn Connection>,
    target: Box<dyn Connection>,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let source = open(&dir.path().join("src.sqlite")).await;
    let target = open(&dir.path().join("tgt.sqlite")).await;
    for (c, upto) in [(&source, 20), (&target, 18)] {
        run(c.as_ref(), SCHEMA).await;
        run(c.as_ref(), &seed(upto)).await;
    }
    run(target.as_ref(), TARGET_CHANGES).await;
    Fixture {
        _dir: dir,
        reg: registry(),
        source,
        target,
    }
}

/// Compares every table of [`TABLES`] into `store`.
async fn compare_all(
    f: &Fixture,
    store: &mut DiffStore,
    options: &DataCompareOptions,
    cancel: &CancellationToken,
) -> Vec<(TableDataDiff, TableDef)> {
    let version = f.target.server_version().clone();
    let mut out = Vec::new();
    for (pair, name) in TABLES.iter().enumerate() {
        let source_table = load(&f.reg, f.source.as_ref(), name).await;
        let target_table = load(&f.reg, f.target.as_ref(), name).await;
        let progress = Mutex::new(Vec::new());
        let on_progress = |p: &oxbase_diff::CompareProgress| {
            progress.lock().unwrap().push(*p);
        };
        let diff = compare_table(
            CompareJob {
                dialects: &f.reg,
                engine: Engine::Sqlite,
                version: &version,
                pair,
                source: f.source.as_ref(),
                target: f.target.as_ref(),
                source_table: &source_table,
                target_table: &target_table,
                choice: None,
                columns: None,
                options,
            },
            store,
            &on_progress,
            cancel.clone(),
        )
        .await
        .unwrap();
        if diff.status == TableCompareStatus::Compared {
            let progress = progress.into_inner().unwrap();
            let last = progress.last().expect("progress reported");
            assert_eq!(last.counts, diff.counts, "{name}");
        }
        out.push((diff, target_table));
    }
    out
}

fn counts(diff: &TableDataDiff) -> [u64; 3] {
    [
        diff.counts.inserts,
        diff.counts.updates,
        diff.counts.deletes,
    ]
}

#[tokio::test]
async fn sqlite_pairs_compare_apply_and_recompare_in_sync() {
    let f = fixture().await;
    let options = DataCompareOptions::default();
    let mut store = DiffStore::new(None).unwrap();
    let diffs = compare_all(&f, &mut store, &options, &CancellationToken::new()).await;
    store.finish().unwrap();

    let data = &diffs[0].0;
    assert_eq!(data.status, TableCompareStatus::Compared);
    assert_eq!(counts(data), [2, 5, 2]);
    assert_eq!(
        data.counts,
        RowCounts {
            source_rows: 20,
            target_rows: 20,
            same: 13,
            inserts: 2,
            updates: 5,
            deletes: 2,
        }
    );
    assert_eq!(data.key, ["id"]);
    assert_eq!(
        data.columns,
        ["id", "name", "amount", "payload", "happened_at", "note"]
    );
    let update_9 = data
        .samples
        .iter()
        .find(|s| s.op == RowOp::Update && s.key == [Some("9".to_owned())])
        .expect("sample of id 9");
    assert_eq!(update_9.changed, ["note"]);
    assert_eq!(update_9.source.as_ref().unwrap()[5], None);
    assert_eq!(update_9.target.as_ref().unwrap()[5].as_deref(), Some("x"));
    let insert_20 = data
        .samples
        .iter()
        .find(|s| s.op == RowOp::Insert && s.key == [Some("20".to_owned())])
        .expect("sample of id 20");
    assert_eq!(
        insert_20.source.as_ref().unwrap()[3].as_deref(),
        Some("0x00ff10")
    );
    assert_eq!(store.rows(0, RowOp::Insert), 2);
    assert_eq!(store.rows(0, RowOp::Update), 5);
    assert_eq!(store.rows(0, RowOp::Delete), 2);

    let big = &diffs[1].0;
    assert_eq!(big.status, TableCompareStatus::Compared);
    assert_eq!(counts(big), [1, 1, 1]);
    assert_eq!(big.counts.source_rows, 100_000);
    assert_eq!(big.counts.target_rows, 100_000);
    assert_eq!(big.chunks_skipped, 0, "SQLite has no chunk hasher");

    assert_eq!(counts(&diffs[2].0), [0, 1, 0]);
    assert_eq!(diffs[2].0.key, ["tenant", "code"]);
    match &diffs[3].0.status {
        TableCompareStatus::Refused { reason } => {
            assert!(reason.contains("choose key columns"), "{reason}");
        }
        other => panic!("m8_nokey: {other:?}"),
    }

    // The preview of m8_data, through the engine's writer and dump literals.
    let quoting = f.reg.quoting(Engine::Sqlite).unwrap();
    let version = f.target.server_version().clone();
    let mut preview = Vec::new();
    for op in [RowOp::Delete, RowOp::Update, RowOp::Insert] {
        let key = row_key(&diffs[0].1, &diffs[0].0.key, None);
        for batch in store.read(0, op, 1_000).unwrap() {
            for st in data_statements(
                &f.reg,
                Engine::Sqlite,
                &version,
                &diffs[0].1,
                &key,
                op,
                &batch.unwrap(),
            )
            .unwrap()
            {
                preview.push(inline_params(quoting.as_ref(), &st).unwrap());
            }
        }
    }
    eprintln!("[m8 data sqlite] preview:\n{}", preview.join(";\n"));
    assert_eq!(preview.len(), 9);
    assert!(preview[0].starts_with("DELETE FROM "), "{}", preview[0]);
    assert!(preview[2].starts_with("UPDATE "), "{}", preview[2]);
    assert!(preview[8].starts_with("INSERT INTO "), "{}", preview[8]);
    assert!(
        preview.iter().any(|s| s.contains("X'00ff10'")),
        "{preview:?}"
    );

    // Apply everything (deletes, updates, inserts) in one transaction.
    let tx = f.target.begin().await.unwrap();
    let mut applied = 0;
    for (pair, (diff, table)) in diffs.iter().enumerate() {
        if diff.status != TableCompareStatus::Compared {
            continue;
        }
        let key = row_key(table, &diff.key, None);
        for op in [RowOp::Delete, RowOp::Update, RowOp::Insert] {
            for batch in store.read(pair, op, 1_000).unwrap() {
                for st in data_statements(
                    &f.reg,
                    Engine::Sqlite,
                    &version,
                    table,
                    &key,
                    op,
                    &batch.unwrap(),
                )
                .unwrap()
                {
                    let done = collect(
                        tx.execute(&st.sql, &st.params, CancellationToken::new())
                            .await
                            .unwrap(),
                    )
                    .await
                    .unwrap_or_else(|e| panic!("{e}: {}", st.sql));
                    assert_eq!(done.rows_affected.iter().sum::<u64>(), 1, "{}", st.sql);
                    applied += 1;
                }
            }
        }
    }
    tx.commit().await.unwrap();
    assert_eq!(applied, 9 + 3 + 1);

    let mut again = DiffStore::new(None).unwrap();
    let after = compare_all(&f, &mut again, &options, &CancellationToken::new()).await;
    for (diff, _) in &after[..3] {
        assert_eq!(diff.status, TableCompareStatus::Compared);
        assert!(
            diff.counts.in_sync(),
            "{:?}: {:?}",
            diff.source,
            diff.counts
        );
    }
    assert_eq!(after[1].0.counts.same, 100_000);
}

#[tokio::test]
async fn every_chunk_size_finds_the_same_differences() {
    let f = fixture().await;
    for chunk_rows in [1, 2, 7, 18, 19, 20, 21, 50] {
        let options = DataCompareOptions {
            chunk_rows,
            sample_rows: 1,
            use_chunk_hash: true,
        };
        let mut store = DiffStore::new(None).unwrap();
        let source_table = load(&f.reg, f.source.as_ref(), "m8_data").await;
        let target_table = load(&f.reg, f.target.as_ref(), "m8_data").await;
        let version = f.target.server_version().clone();
        let diff = compare_table(
            CompareJob {
                dialects: &f.reg,
                engine: Engine::Sqlite,
                version: &version,
                pair: 0,
                source: f.source.as_ref(),
                target: f.target.as_ref(),
                source_table: &source_table,
                target_table: &target_table,
                choice: Some(&KeyChoice::Primary),
                columns: Some(&["note".to_owned(), "name".to_owned()]),
                options: &options,
            },
            &mut store,
            &no_progress,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        // Only name and note compared: ids 3 (name), 9 and 10 (note) differ.
        assert_eq!(counts(&diff), [2, 3, 2], "chunk_rows {chunk_rows}");
        assert_eq!(diff.columns, ["id", "note", "name"]);
        assert_eq!(diff.counts.source_rows, 20, "chunk_rows {chunk_rows}");
        assert_eq!(diff.counts.target_rows, 20, "chunk_rows {chunk_rows}");
        // One sample per kind.
        assert_eq!(diff.samples.len(), 3, "chunk_rows {chunk_rows}");
        store.finish().unwrap();
        let stored: usize = store
            .read(0, RowOp::Update, 2)
            .unwrap()
            .map(|b| b.unwrap().num_rows())
            .sum();
        assert_eq!(stored, 3);
    }
}

fn job<'a>(
    f: &'a Fixture,
    version: &'a ServerVersion,
    tables: (&'a TableDef, &'a TableDef),
    choice: Option<&'a KeyChoice>,
    options: &'a DataCompareOptions,
) -> CompareJob<'a> {
    CompareJob {
        dialects: &f.reg,
        engine: Engine::Sqlite,
        version,
        pair: 0,
        source: f.source.as_ref(),
        target: f.target.as_ref(),
        source_table: tables.0,
        target_table: tables.1,
        choice,
        columns: None,
        options,
    }
}

fn no_progress(_: &oxbase_diff::CompareProgress) {}

#[tokio::test]
async fn user_keys_and_refusals() {
    let f = fixture().await;
    let version = f.target.server_version().clone();
    let options = DataCompareOptions::default();
    let source_table = load(&f.reg, f.source.as_ref(), "m8_nokey").await;
    let target_table = load(&f.reg, f.target.as_ref(), "m8_nokey").await;
    let tables = (&source_table, &target_table);
    // Chosen key columns make a keyless table comparable.
    let chosen = KeyChoice::Columns {
        columns: vec!["a".into()],
    };
    let mut store = DiffStore::new(None).unwrap();
    let diff = compare_table(
        job(&f, &version, tables, Some(&chosen), &options),
        &mut store,
        &no_progress,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(diff.status, TableCompareStatus::Compared);
    assert!(diff.counts.in_sync());
    assert_eq!(diff.counts.same, 1);

    // A chosen key that is not unique fails the table.
    run(
        f.target.as_ref(),
        "INSERT INTO m8_nokey VALUES (1, 'again')",
    )
    .await;
    let diff = compare_table(
        job(&f, &version, tables, Some(&chosen), &options),
        &mut store,
        &no_progress,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    match diff.status {
        TableCompareStatus::Failed { message } => {
            assert!(message.contains("duplicate key (1)"), "{message}");
        }
        other => panic!("{other:?}"),
    }

    // A cancelled compare reports Cancelled.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let diff = compare_table(
        job(&f, &version, tables, Some(&chosen), &options),
        &mut store,
        &no_progress,
        cancel,
    )
    .await
    .unwrap();
    assert_eq!(diff.status, TableCompareStatus::Cancelled);

    // Key types must match on both sides.
    let mut other = target_table.clone();
    other.columns[0].data_type = oxbase_types::DataType::Text;
    let diff = compare_table(
        job(
            &f,
            &version,
            (&source_table, &other),
            Some(&chosen),
            &options,
        ),
        &mut store,
        &no_progress,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(&diff.status, TableCompareStatus::Refused { reason } if reason.contains("type")),
        "{:?}",
        diff.status
    );
}
