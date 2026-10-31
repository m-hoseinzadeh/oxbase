//! Driver tests against real SQLite databases (in memory and in temporary files). No Docker.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_array::{Array, BinaryArray, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::DataType;
use futures::StreamExt;
use oxbase_driver::{
    CancellationToken, Collected, ConnectParams, Connection, Driver, DriverRegistry, ResultItem,
    ResultStream, SshAuth, SshOptions, TlsMode, TlsOptions, collect,
};
use oxbase_sqlite::{
    DECL_TYPE_METADATA_KEY, OPT_BUSY_TIMEOUT_MS, OPT_CREATE_IF_MISSING, OPT_READ_ONLY, SqliteDriver,
};
use oxbase_types::{ErrorCategory, Value};

/// A long-running statement: counts an endless recursive CTE.
const ENDLESS: &str =
    "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n) SELECT count(*) FROM n";

async fn memory() -> Box<dyn Connection> {
    SqliteDriver
        .connect(&ConnectParams::sqlite(":memory:"))
        .await
        .unwrap()
}

async fn stream(c: &dyn Connection, sql: &str) -> ResultStream {
    c.execute(sql, &[], CancellationToken::new()).await.unwrap()
}

async fn run(c: &dyn Connection, sql: &str) -> Collected {
    collect(stream(c, sql).await).await.unwrap()
}

async fn run_err(c: &dyn Connection, sql: &str) -> oxbase_types::Error {
    collect(stream(c, sql).await).await.unwrap_err()
}

async fn scalar(c: &dyn Connection, sql: &str) -> i64 {
    let got = run(c, sql).await;
    ints(&got.result_sets[0].batches[0], 0)[0].unwrap()
}

fn ints(b: &RecordBatch, col: usize) -> Vec<Option<i64>> {
    let a = b.column(col).as_any().downcast_ref::<Int64Array>().unwrap();
    a.iter().collect()
}

fn floats(b: &RecordBatch, col: usize) -> Vec<Option<f64>> {
    let a = b
        .column(col)
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap();
    a.iter().collect()
}

fn strings(b: &RecordBatch, col: usize) -> Vec<Option<String>> {
    let a = b
        .column(col)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    a.iter().map(|s| s.map(str::to_owned)).collect()
}

fn types(got: &Collected, set: usize) -> Vec<DataType> {
    got.result_sets[set]
        .schema
        .fields()
        .iter()
        .map(|f| f.data_type().clone())
        .collect()
}

#[tokio::test]
async fn streams_200k_rows_in_several_batches() {
    let c = memory().await;
    let sql = "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 200000) \
               SELECT x, 'row ' || x AS label, x / 2.0 AS half FROM n";
    let mut s = stream(c.as_ref(), sql).await;
    let Some(Ok(ResultItem::Schema(schema))) = s.next().await else {
        panic!("first item must be the schema");
    };
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names, ["x", "label", "half"]);
    let (mut batches, mut rows, mut sum) = (0_usize, 0_usize, 0_i64);
    let mut last_label = None;
    while let Some(item) = s.next().await {
        let ResultItem::Batch(b) = item.unwrap() else {
            panic!("only batches follow the schema");
        };
        assert_eq!(b.schema(), schema);
        batches += 1;
        rows += b.num_rows();
        sum += ints(&b, 0).into_iter().map(Option::unwrap).sum::<i64>();
        last_label = strings(&b, 1).pop().flatten();
        let (x, half) = (ints(&b, 0)[0].unwrap(), floats(&b, 2)[0].unwrap());
        assert_eq!(half * 2.0, f64::from(i32::try_from(x).unwrap()));
    }
    assert_eq!(rows, 200_000);
    assert!(batches > 1, "expected several batches, got {batches}");
    assert_eq!(sum, 200_000 * 200_001 / 2);
    assert_eq!(last_label.as_deref(), Some("row 200000"));
    assert_eq!(
        schema
            .fields()
            .iter()
            .map(|f| f.data_type().clone())
            .collect::<Vec<_>>(),
        [DataType::Int64, DataType::Utf8, DataType::Float64]
    );
}

#[tokio::test]
async fn token_cancels_a_long_recursive_cte_within_2s() {
    let c = memory().await;
    let token = CancellationToken::new();
    let s = c.execute(ENDLESS, &[], token.clone()).await.unwrap();
    let canceller = token.clone();
    let cancelled_at = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        canceller.cancel();
        Instant::now()
    });
    let result = tokio::time::timeout(Duration::from_secs(5), collect(s))
        .await
        .expect("the stream must end after the cancel");
    let elapsed = cancelled_at.await.unwrap().elapsed();
    assert!(result.unwrap_err().is_cancelled());
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    // The connection stays usable (and the worker really stopped: it runs one job at a time).
    let next = tokio::time::timeout(Duration::from_secs(2), scalar(c.as_ref(), "SELECT 41 + 1"))
        .await
        .expect("the statement kept running after the cancel");
    assert_eq!(next, 42);
}

#[tokio::test]
async fn cancel_running_stops_a_long_recursive_cte_within_2s() {
    let c: Arc<dyn Connection> = Arc::from(memory().await);
    let s = stream(c.as_ref(), ENDLESS).await;
    let c2 = c.clone();
    let cancelled_at = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c2.cancel_running().await.unwrap();
        Instant::now()
    });
    let result = tokio::time::timeout(Duration::from_secs(5), collect(s))
        .await
        .expect("the stream must end after cancel_running");
    let elapsed = cancelled_at.await.unwrap().elapsed();
    assert!(result.unwrap_err().is_cancelled());
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    let next = tokio::time::timeout(Duration::from_secs(2), scalar(c.as_ref(), "SELECT 7"))
        .await
        .expect("the statement kept running after cancel_running");
    assert_eq!(next, 7);
    // Nothing running: cancel_running is a no-op and does not affect the next statement.
    c.cancel_running().await.unwrap();
    assert_eq!(scalar(c.as_ref(), "SELECT 8").await, 8);
}

#[tokio::test]
async fn dropping_or_cancelling_an_unpolled_stream_stops_the_statement() {
    let c = memory().await;
    // Cancel while nobody polls: the worker must stop on its own.
    let token = CancellationToken::new();
    let s = c.execute(ENDLESS, &[], token.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    token.cancel();
    let next = tokio::time::timeout(Duration::from_secs(2), scalar(c.as_ref(), "SELECT 1"))
        .await
        .expect("the endless statement was not interrupted");
    assert_eq!(next, 1);
    drop(s);

    // Dropping the stream cancels too.
    let s = stream(c.as_ref(), ENDLESS).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(s);
    let next = tokio::time::timeout(Duration::from_secs(2), scalar(c.as_ref(), "SELECT 2"))
        .await
        .expect("dropping the stream did not interrupt the statement");
    assert_eq!(next, 2);

    // An already-cancelled token sends nothing.
    let token = CancellationToken::new();
    token.cancel();
    let err = c.execute("SELECT 1", &[], token).await.err().unwrap();
    assert!(err.is_cancelled());
}

#[tokio::test]
async fn runs_multi_statement_scripts() {
    let c = memory().await;
    let script = "
        CREATE TABLE t(id INTEGER PRIMARY KEY, note TEXT); -- a comment; with a semicolon
        INSERT INTO t(note) VALUES ('a;b'), ('c'); /* ; ; */
        SELECT id, note FROM t ORDER BY id;
        UPDATE t SET note = 'x;' WHERE id = 2;
        CREATE TRIGGER tr AFTER INSERT ON t BEGIN
            UPDATE t SET note = note || ';' WHERE id = new.id;
        END;
        INSERT INTO t(note) VALUES ('d');
        SELECT note FROM t ORDER BY id;
        -- trailing comment only
    ";
    let mut kinds = Vec::new();
    let mut s = stream(c.as_ref(), script).await;
    while let Some(item) = s.next().await {
        kinds.push(match item.unwrap() {
            ResultItem::Schema(_) => "schema".to_owned(),
            ResultItem::Batch(b) => format!("batch{}", b.num_rows()),
            ResultItem::RowsAffected(n) => format!("rows{n}"),
            ResultItem::Notice(n) => panic!("unexpected notice {n}"),
        });
    }
    assert_eq!(
        kinds,
        [
            "rows0", "rows2", "schema", "batch2", "rows1", "rows0", "rows1", "schema", "batch3"
        ]
    );
    let got = run(c.as_ref(), "SELECT note FROM t ORDER BY id").await;
    assert_eq!(
        strings(&got.result_sets[0].batches[0], 0),
        [
            Some("a;b".to_owned()),
            Some("x;".to_owned()),
            Some("d;".to_owned())
        ]
    );

    // An error stops the script: earlier statements ran, later ones did not.
    let mut s = stream(
        c.as_ref(),
        "INSERT INTO t(note) VALUES ('e'); SELECT * FROM missing; INSERT INTO t(note) VALUES ('f')",
    )
    .await;
    assert!(matches!(
        s.next().await,
        Some(Ok(ResultItem::RowsAffected(1)))
    ));
    let err = s.next().await.unwrap().unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Sql);
    assert!(err.to_string().contains("missing"), "{err}");
    assert!(s.next().await.is_none());
    assert_eq!(
        scalar(
            c.as_ref(),
            "SELECT count(*) FROM t WHERE note IN ('e;', 'f;')"
        )
        .await,
        1
    );

    // A syntax error is a SQL error with SQLite's code.
    let err = run_err(c.as_ref(), "SELEC 1").await;
    assert_eq!(err.sql_code(), Some("SQLITE_ERROR"));
}

#[tokio::test]
async fn maps_declared_types_and_mixed_columns() {
    let c = memory().await;
    run(
        c.as_ref(),
        "CREATE TABLE m(v);
         INSERT INTO m VALUES (1), ('two'), (3.5), (x'00ff'), (NULL);
         CREATE TABLE d(i INTEGER, r REAL, t VARCHAR(10), b BLOB, n NUMERIC, flag BOOLEAN,
                        dt DATETIME, dec DECIMAL(10,2));",
    )
    .await;

    // Mixed-type column -> Utf8.
    let got = run(c.as_ref(), "SELECT v FROM m ORDER BY rowid").await;
    assert_eq!(types(&got, 0), [DataType::Utf8]);
    assert_eq!(
        strings(&got.result_sets[0].batches[0], 0),
        [
            Some("1".to_owned()),
            Some("two".to_owned()),
            Some("3.5".to_owned()),
            Some("0x00ff".to_owned()),
            None
        ]
    );

    // Empty table: the declared types decide.
    let got = run(c.as_ref(), "SELECT * FROM d").await;
    assert_eq!(
        types(&got, 0),
        [
            DataType::Int64,
            DataType::Float64,
            DataType::Utf8,
            DataType::Binary,
            DataType::Float64,
            DataType::Int64,
            DataType::Utf8,
            DataType::Float64
        ]
    );
    assert_eq!(got.result_sets[0].num_rows(), 0);
    let field = got.result_sets[0].schema.field(2).clone();
    assert_eq!(
        field
            .metadata()
            .get(DECL_TYPE_METADATA_KEY)
            .map(String::as_str),
        Some("VARCHAR(10)")
    );

    // With values: storage classes decide.
    run(
        c.as_ref(),
        "INSERT INTO d VALUES (1, 2.5, 'x', x'01', 10, 1, '2026-10-31 12:00:00', 9.75)",
    )
    .await;
    let got = run(c.as_ref(), "SELECT * FROM d").await;
    assert_eq!(
        types(&got, 0),
        [
            DataType::Int64,
            DataType::Float64,
            DataType::Utf8,
            DataType::Binary,
            DataType::Float64,
            DataType::Int64,
            DataType::Utf8,
            DataType::Float64
        ]
    );
    let b = &got.result_sets[0].batches[0];
    assert_eq!(floats(b, 4), [Some(10.0)]);
    let blob = b.column(3).as_any().downcast_ref::<BinaryArray>().unwrap();
    assert_eq!(blob.value(0), [1_u8].as_slice());
    assert_eq!(floats(b, 7), [Some(9.75)]);

    // Integers and reals in one column -> Float64; expressions without values -> Utf8.
    let got = run(c.as_ref(), "SELECT 1 AS a UNION ALL SELECT 2.5").await;
    assert_eq!(types(&got, 0), [DataType::Float64]);
    let got = run(c.as_ref(), "SELECT NULL AS empty").await;
    assert_eq!(types(&got, 0), [DataType::Utf8]);

    // A value after the first batch that does not fit becomes NULL, with a warning.
    let got = run(
        c.as_ref(),
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 5000) \
         SELECT CASE WHEN x = 5000 THEN 'oops' ELSE x END AS v FROM n",
    )
    .await;
    assert_eq!(types(&got, 0), [DataType::Int64]);
    let set = &got.result_sets[0];
    assert_eq!(set.num_rows(), 5000);
    let last = set.batches.last().unwrap();
    assert_eq!(ints(last, 0).pop(), Some(None));
    assert_eq!(got.notices.len(), 1, "{:?}", got.notices);
    assert!(
        got.notices[0].message.contains("\"v\""),
        "{}",
        got.notices[0]
    );
}

#[tokio::test]
async fn numeric_columns_keep_fractions_after_whole_values() {
    let c = memory().await;
    run(
        c.as_ref(),
        "CREATE TABLE price(p DECIMAL(10,2));
         INSERT INTO price WITH RECURSIVE n(x) AS
             (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 3000) SELECT x FROM n;
         INSERT INTO price VALUES (9.75);",
    )
    .await;
    let got = run(c.as_ref(), "SELECT p FROM price ORDER BY rowid").await;
    assert_eq!(types(&got, 0), [DataType::Float64]);
    let set = &got.result_sets[0];
    assert_eq!(set.num_rows(), 3001);
    let values: Vec<Option<f64>> = set.batches.iter().flat_map(|b| floats(b, 0)).collect();
    assert!(values.iter().all(Option::is_some), "no value became NULL");
    assert_eq!(values.last(), Some(&Some(9.75)));
    assert!(got.notices.is_empty(), "{:?}", got.notices);
}

#[tokio::test]
async fn slow_first_row_still_samples_many_rows_for_types() {
    let c = memory().await;
    // The scalar subquery runs before the first row (and once): the first row comes late. Row 1
    // is a whole number, later rows are not; a one-row sample would pick Int64 and lose them.
    let sql = "WITH RECURSIVE \
               big(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM big WHERE x < 3000000), \
               small(y) AS (SELECT 1 UNION ALL SELECT y + 1 FROM small WHERE y < 3000) \
               SELECT CASE WHEN y = 1 THEN 1 ELSE y + 0.5 END \
                      + 0 * (SELECT count(*) FROM big) AS v \
               FROM small";
    let got = run(c.as_ref(), sql).await;
    assert_eq!(types(&got, 0), [DataType::Float64]);
    let set = &got.result_sets[0];
    assert_eq!(set.num_rows(), 3000);
    assert!(got.notices.is_empty(), "{:?}", got.notices);
}

#[tokio::test]
async fn a_held_but_unread_stream_does_not_block_cancel_or_close() {
    const MANY: &str = "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n \
                        WHERE x < 1000000) SELECT x, 'row ' || x FROM n";
    let c = memory().await;
    // The worker fills the channel and then waits for room; cancel_running must free it.
    let mut s = stream(c.as_ref(), MANY).await;
    assert!(matches!(s.next().await, Some(Ok(ResultItem::Schema(_)))));
    tokio::time::sleep(Duration::from_millis(300)).await;
    c.cancel_running().await.unwrap();
    let next = tokio::time::timeout(Duration::from_secs(2), scalar(c.as_ref(), "SELECT 1"))
        .await
        .expect("cancel_running did not free a worker waiting on a held stream");
    assert_eq!(next, 1);
    // The held stream reports the cancel.
    assert!(s.next().await.unwrap().unwrap_err().is_cancelled());
    assert!(s.next().await.is_none());

    // close() must return while a stream is held and unread.
    let mut s = stream(c.as_ref(), MANY).await;
    assert!(matches!(s.next().await, Some(Ok(ResultItem::Schema(_)))));
    tokio::time::sleep(Duration::from_millis(300)).await;
    tokio::time::timeout(Duration::from_secs(2), c.close())
        .await
        .expect("close() hung on a held stream")
        .unwrap();
    drop(s);
}

#[tokio::test]
async fn cancel_stops_a_statement_waiting_for_a_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("busy.sqlite");
    let holder = SqliteDriver
        .connect(&ConnectParams::sqlite(&path))
        .await
        .unwrap();
    run(holder.as_ref(), "CREATE TABLE t(x INTEGER)").await;
    let with_timeout = |ms: &str| {
        let mut p = ConnectParams::sqlite(&path);
        p.options.insert(OPT_BUSY_TIMEOUT_MS.into(), ms.into());
        p
    };
    // Opened before the lock is taken (opening reads the file header).
    let waiter = SqliteDriver.connect(&with_timeout("60000")).await.unwrap();
    let quick = SqliteDriver.connect(&with_timeout("100")).await.unwrap();
    run(holder.as_ref(), "BEGIN EXCLUSIVE").await;

    let token = CancellationToken::new();
    let s = waiter
        .execute("INSERT INTO t VALUES (1)", &[], token.clone())
        .await
        .unwrap();
    let canceller = token.clone();
    let cancelled_at = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        canceller.cancel();
        Instant::now()
    });
    let result = tokio::time::timeout(Duration::from_secs(5), collect(s))
        .await
        .expect("the lock wait did not stop after the cancel");
    let elapsed = cancelled_at.await.unwrap().elapsed();
    assert!(result.unwrap_err().is_cancelled());
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");

    // Without a cancel, the busy timeout still applies.
    let started = Instant::now();
    let err = run_err(quick.as_ref(), "INSERT INTO t VALUES (2)").await;
    assert_eq!(err.sql_code(), Some("SQLITE_BUSY"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(2));

    run(holder.as_ref(), "ROLLBACK").await;
    run(waiter.as_ref(), "INSERT INTO t VALUES (3)").await;
    assert_eq!(scalar(holder.as_ref(), "SELECT count(*) FROM t").await, 1);
    for c in [holder, waiter, quick] {
        c.close().await.unwrap();
    }
}

#[tokio::test]
async fn binds_typed_parameters() {
    let c = memory().await;
    let params = [
        Value::Int(41),
        Value::Text("hi".into()),
        Value::Bytes(vec![0xde, 0xad]),
        Value::Float(1.25),
        Value::Null,
        Value::Bool(true),
    ];
    let s = c
        .execute(
            "SELECT ?1 + 1 AS a, ?2 AS b, ?3 AS c, ?4 AS d, typeof(?5) AS e, ?6 AS f",
            &params,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let got = collect(s).await.unwrap();
    assert_eq!(
        types(&got, 0),
        [
            DataType::Int64,
            DataType::Utf8,
            DataType::Binary,
            DataType::Float64,
            DataType::Utf8,
            DataType::Int64
        ]
    );
    let b = &got.result_sets[0].batches[0];
    assert_eq!(ints(b, 0), [Some(42)]);
    assert_eq!(strings(b, 1), [Some("hi".to_owned())]);
    assert_eq!(strings(b, 4), [Some("null".to_owned())]);
    assert_eq!(ints(b, 5), [Some(1)]);

    // Every statement of a script binds from the same list.
    let s = c
        .execute(
            "CREATE TABLE p(x INTEGER, y TEXT); INSERT INTO p VALUES (?1, ?2); \
             SELECT x, y FROM p WHERE x = ?1",
            &[Value::Int(5), Value::Text("five".into())],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let got = collect(s).await.unwrap();
    assert_eq!(got.rows_affected, [0, 1]);
    assert_eq!(
        strings(&got.result_sets[0].batches[0], 1),
        [Some("five".to_owned())]
    );

    // Too few and too many parameters.
    let too_few = c
        .execute("SELECT ?1, ?2", &[Value::Int(1)], CancellationToken::new())
        .await
        .unwrap();
    let err = collect(too_few).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    let too_many = c
        .execute(
            "SELECT ?1",
            &[Value::Int(1), Value::Int(2)],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let err = collect(too_many).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);

    // A count mismatch fails before anything runs when every statement can be prepared...
    run(c.as_ref(), "CREATE TABLE q(x INTEGER)").await;
    for (sql, params) in [
        (
            "INSERT INTO q VALUES (?1)",
            vec![Value::Int(1), Value::Int(2)],
        ),
        (
            "INSERT INTO q VALUES (1); SELECT ?1, ?2",
            vec![Value::Int(1)],
        ),
    ] {
        let s = c
            .execute(sql, &params, CancellationToken::new())
            .await
            .unwrap();
        let err = collect(s).await.unwrap_err();
        assert_eq!(err.category(), ErrorCategory::InvalidInput, "{sql}");
    }
    assert_eq!(scalar(c.as_ref(), "SELECT count(*) FROM q").await, 0);
    // ...and, documented, only after the script when a statement depends on earlier DDL.
    let s = c
        .execute(
            "CREATE TABLE q2(x INTEGER); INSERT INTO q2 VALUES (?1)",
            &[Value::Int(1), Value::Int(2)],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let err = collect(s).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert_eq!(scalar(c.as_ref(), "SELECT count(*) FROM q2").await, 1);
}

#[tokio::test]
async fn transactions_commit_roll_back_and_roll_back_on_drop() {
    let c = memory().await;
    run(c.as_ref(), "CREATE TABLE r(x INTEGER)").await;
    let count = "SELECT count(*) FROM r";

    let tx = c.begin().await.unwrap();
    let got = collect(
        tx.execute(
            "INSERT INTO r VALUES (1), (2)",
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(got.rows_affected, [2]);
    assert_eq!(
        scalar(c.as_ref(), count).await,
        2,
        "visible inside the transaction"
    );
    tx.rollback().await.unwrap();
    assert_eq!(scalar(c.as_ref(), count).await, 0);

    let tx = c.begin().await.unwrap();
    let s = tx
        .execute("INSERT INTO r VALUES (3)", &[], CancellationToken::new())
        .await
        .unwrap();
    collect(s).await.unwrap();
    // One transaction at a time.
    assert_eq!(
        c.begin().await.err().unwrap().category(),
        ErrorCategory::Sql
    );
    tx.commit().await.unwrap();
    assert_eq!(scalar(c.as_ref(), count).await, 1);

    let tx = c.begin().await.unwrap();
    let s = tx
        .execute("INSERT INTO r VALUES (4)", &[], CancellationToken::new())
        .await
        .unwrap();
    collect(s).await.unwrap();
    drop(tx);
    assert_eq!(
        scalar(c.as_ref(), count).await,
        1,
        "dropped transaction rolled back"
    );

    // A transaction ended by a statement cannot be used (or committed) afterwards.
    let tx = c.begin().await.unwrap();
    let s = tx
        .execute("ROLLBACK", &[], CancellationToken::new())
        .await
        .unwrap();
    collect(s).await.unwrap();
    let s = tx
        .execute("INSERT INTO r VALUES (5)", &[], CancellationToken::new())
        .await
        .unwrap();
    let err = collect(s).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert_eq!(
        tx.commit().await.unwrap_err().category(),
        ErrorCategory::InvalidInput
    );
    assert_eq!(scalar(c.as_ref(), count).await, 1);
}

#[tokio::test]
async fn read_only_rejects_writes_and_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro.sqlite");

    // Missing file without create.
    let mut p = ConnectParams::sqlite(&path);
    p.options
        .insert(OPT_CREATE_IF_MISSING.into(), "false".into());
    let err = SqliteDriver.connect(&p).await.err().unwrap();
    assert_eq!(err.category(), ErrorCategory::NotFound, "{err}");
    let mut ro = ConnectParams::sqlite(&path);
    ro.options.insert(OPT_READ_ONLY.into(), "true".into());
    let err = SqliteDriver.connect(&ro).await.err().unwrap();
    assert_eq!(err.category(), ErrorCategory::NotFound, "{err}");

    let rw = SqliteDriver
        .connect(&ConnectParams::sqlite(&path))
        .await
        .unwrap();
    run(
        rw.as_ref(),
        "CREATE TABLE t(x INTEGER); INSERT INTO t VALUES (1)",
    )
    .await;
    rw.close().await.unwrap();

    let c = SqliteDriver.connect(&ro).await.unwrap();
    assert_eq!(scalar(c.as_ref(), "SELECT x FROM t").await, 1);
    for write in [
        "INSERT INTO t VALUES (2)",
        "UPDATE t SET x = 3",
        "CREATE TABLE u(y)",
    ] {
        let err = run_err(c.as_ref(), write).await;
        assert_eq!(err.sql_code(), Some("SQLITE_READONLY"), "{write}: {err}");
    }
    assert_eq!(scalar(c.as_ref(), "SELECT count(*) FROM t").await, 1);
}

#[tokio::test]
async fn reports_version_and_rejects_ssh_and_tls() {
    let c = memory().await;
    assert!(c.server_version().at_least(3, 40), "{}", c.server_version());
    assert!(!c.tls_in_use());
    let got = run(c.as_ref(), "SELECT sqlite_version()").await;
    assert_eq!(
        strings(&got.result_sets[0].batches[0], 0)[0].as_deref(),
        Some(c.server_version().raw())
    );

    let ssh = ConnectParams::sqlite(":memory:").with_ssh(SshOptions::new(
        "bastion",
        "ops",
        SshAuth::Agent,
    ));
    let err = SqliteDriver.connect(&ssh).await.err().unwrap();
    assert_eq!(err.category(), ErrorCategory::Unsupported);
    let tls = ConnectParams::sqlite(":memory:").with_tls(TlsOptions::new(TlsMode::VerifyFull));
    let err = SqliteDriver.connect(&tls).await.err().unwrap();
    assert_eq!(err.category(), ErrorCategory::Unsupported);
}

#[tokio::test]
async fn close_and_registry() {
    let mut registry = DriverRegistry::new();
    registry.register(Arc::new(SqliteDriver));
    let c = registry
        .connect(&ConnectParams::sqlite(":memory:"))
        .await
        .unwrap();
    assert_eq!(scalar(c.as_ref(), "SELECT 3").await, 3);
    c.close().await.unwrap();
    c.close().await.unwrap();
    let err = c
        .execute("SELECT 1", &[], CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert_eq!(err.category(), ErrorCategory::Connection);
    assert_eq!(
        c.begin().await.err().unwrap().category(),
        ErrorCategory::Connection
    );
}
