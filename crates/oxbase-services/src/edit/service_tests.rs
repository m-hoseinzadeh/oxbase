//! `EditService` and the M3 query additions with a scripted in-memory driver (no Docker): the
//! bound table-data SELECT, editability and every read-only reason, apply (validation without
//! touching the database, one transaction, read-back, conflicts with 0 and 2 rows, server
//! errors, per-edit statuses, the overlay used by later edits and forms, refused while
//! running), `run_bound` events / paging / history, `run_session`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arrow_array::{ArrayRef, Int64Array, StringArray};
use arrow_schema::{DataType as ArrowType, Field, Schema, SchemaRef};
use async_trait::async_trait;
use futures::{StreamExt, stream};
use oxbase_dialect::sql::SqlWriter;
use oxbase_dialect::{Fetcher, PgQuoting, write_key_predicate};
use oxbase_driver::{ConnectParams, Connection, Driver, DriverRegistry, ResultItem, ResultStream};
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_store::{
    ConnectionProfile, HistoryQuery, HistoryRepository, MemoryHistoryRepository, MemorySecretStore,
    ProfileAddress,
};
use oxbase_types::{
    BoundStatement, Extras, Filter, FilterOp, GeneratedColumn, IndexColumn, IndexDef,
    ObjectSummary, PrimaryKeyDef, SortKey, ViewDef,
};
use pretty_assertions::assert_eq;
use tokio::sync::mpsc;

use super::*;
use crate::history::HistoryService;
use crate::query::{EditorId, QueryConfig, RunEvent};
use crate::{ConnectionManager, ProvidedSecrets, SessionId};

// ---------------------------------------------------------------------------------------------
// Scripted driver
// ---------------------------------------------------------------------------------------------

/// What the driver does, shared by all its connections.
#[derive(Default)]
struct State {
    /// Every statement with its parameters (`sql [p1, p2]`), plus `BEGIN` / `COMMIT` /
    /// `ROLLBACK`.
    log: Vec<String>,
    /// Rows the next writes affect (default 1).
    affected: VecDeque<u64>,
}

type Shared = Arc<Mutex<State>>;

/// Answers by the first word: `SELECT` → the rows `(1, a, 2), (2, b, 4), (3, c, NULL)` of
/// `(id, code, total)` (after 30 s, unless cancelled, when the SQL mentions `slow`);
/// `READBACK` → one row `(<param 1>, read, 42)`; `INSERT` / `UPDATE` / `DELETE` → the next
/// queued affected count (default 1) as `RowsAffected`, or as that many returned rows when the
/// SQL says `RETURNING`; a parameter `FAIL` → an SQL error; anything else `RowsAffected(0)`.
struct TableDriver {
    state: Shared,
}

#[async_trait]
impl Driver for TableDriver {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        lock(&self.state).log.push(format!(
            "connect {}",
            params.database.as_deref().unwrap_or("-")
        ));
        Ok(Box::new(TableConnection {
            state: Arc::clone(&self.state),
            database: params.database.clone(),
            version: ServerVersion::new(17, 0, 0),
            closed: AtomicBool::new(false),
        }))
    }
}

struct TableConnection {
    state: Shared,
    database: Option<String>,
    version: ServerVersion,
    closed: AtomicBool,
}

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", ArrowType::Int64, true),
        Field::new("code", ArrowType::Utf8, true),
        Field::new("total", ArrowType::Int64, true),
    ]))
}

fn batch(ids: Vec<i64>, codes: Vec<&str>, totals: Vec<Option<i64>>) -> RecordBatch {
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(ids)),
        Arc::new(StringArray::from(codes)),
        Arc::new(Int64Array::from(totals)),
    ];
    RecordBatch::try_new(schema(), columns).unwrap()
}

fn render(sql: &str, params: &[Value]) -> String {
    if params.is_empty() {
        return sql.to_owned();
    }
    let params: Vec<String> = params.iter().map(ToString::to_string).collect();
    format!("{sql} [{}]", params.join(", "))
}

fn answer(
    state: &Shared,
    sql: &str,
    params: &[Value],
    cancel: CancellationToken,
) -> Result<ResultStream> {
    lock(state).log.push(render(sql, params));
    if params.iter().any(|p| p.as_str() == Some("FAIL")) {
        return Err(Error::sql(Some("23514"), "the server refused the value"));
    }
    let head = sql
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let items: Vec<Result<ResultItem>> = match head.as_str() {
        "SELECT" => vec![
            Ok(ResultItem::Schema(schema())),
            Ok(ResultItem::Batch(batch(
                vec![1, 2, 3],
                vec!["a", "b", "c"],
                vec![Some(2), Some(4), None],
            ))),
        ],
        "READBACK" => {
            let id = params.first().and_then(Value::as_i64).unwrap_or(-1);
            vec![
                Ok(ResultItem::Schema(schema())),
                Ok(ResultItem::Batch(batch(
                    vec![id],
                    vec!["read"],
                    vec![Some(42)],
                ))),
            ]
        }
        "INSERT" | "UPDATE" | "DELETE" => {
            let n = lock(state).affected.pop_front().unwrap_or(1);
            if sql.contains("RETURNING") {
                let count = usize::try_from(n).unwrap();
                vec![
                    Ok(ResultItem::Schema(schema())),
                    Ok(ResultItem::Batch(batch(
                        vec![7; count],
                        vec!["ret"; count],
                        vec![Some(14); count],
                    ))),
                ]
            } else {
                vec![Ok(ResultItem::RowsAffected(n))]
            }
        }
        _ => vec![Ok(ResultItem::RowsAffected(0))],
    };
    let delay = if sql.contains("slow") {
        Duration::from_secs(30)
    } else {
        Duration::ZERO
    };
    Ok(stream::iter(items)
        .then(move |item| {
            let cancel = cancel.clone();
            async move {
                if !delay.is_zero() {
                    tokio::select! {
                        () = cancel.cancelled() => return Err(Error::Cancelled),
                        () = tokio::time::sleep(delay) => {}
                    }
                }
                item
            }
        })
        .boxed())
}

#[async_trait]
impl Connection for TableConnection {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    fn server_version(&self) -> &ServerVersion {
        &self.version
    }

    fn current_database(&self) -> Option<String> {
        self.database.clone()
    }

    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::connection("closed"));
        }
        answer(&self.state, sql, params, cancel)
    }

    async fn cancel_running(&self) -> Result<()> {
        Ok(())
    }

    async fn begin(&self) -> Result<Box<dyn Tx>> {
        lock(&self.state).log.push("BEGIN".into());
        Ok(Box::new(TableTx {
            state: Arc::clone(&self.state),
            done: false,
        }))
    }

    async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct TableTx {
    state: Shared,
    done: bool,
}

#[async_trait]
impl Tx for TableTx {
    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        answer(&self.state, sql, params, cancel)
    }

    async fn commit(mut self: Box<Self>) -> Result<()> {
        self.done = true;
        lock(&self.state).log.push("COMMIT".into());
        Ok(())
    }

    async fn rollback(mut self: Box<Self>) -> Result<()> {
        self.done = true;
        lock(&self.state).log.push("ROLLBACK".into());
        Ok(())
    }
}

impl Drop for TableTx {
    fn drop(&mut self) {
        if !self.done {
            lock(&self.state).log.push("ROLLBACK (dropped)".into());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Dialect cells: a table fetcher and a toy DML writer
// ---------------------------------------------------------------------------------------------

/// Tables of schema `public`: `t` (PK `id`, unique `code`, generated `total`), `slow` (the same
/// shape, its rows take 30 s), `uq` (no PK, NOT NULL unique `code`), `nokey` (unique on a
/// nullable column), `extra` (PK on a column the result lacks); view `v`.
struct Tables;

fn table_def(name: &str) -> Option<TableDef> {
    let mut def = TableDef::new(ObjectRef::table(Some("db1"), Some("public"), name));
    let mut id = ColumnDef::new("id", DataType::INT64).not_null();
    id.auto_increment = true;
    let code = ColumnDef::new("code", DataType::VarChar { length: Some(20) }).not_null();
    let mut total = ColumnDef::new("total", DataType::INT64);
    total.generated = Some(GeneratedColumn {
        expression: "id * 2".into(),
        stored: true,
    });
    let unique_code = IndexDef {
        name: format!("{name}_code_key"),
        columns: vec![IndexColumn::column("code")],
        unique: true,
        is_constraint: true,
        predicate: None,
        extras: Extras::new(),
    };
    match name {
        "t" | "slow" => {
            def.columns = vec![id, code, total];
            def.primary_key = Some(PrimaryKeyDef {
                name: None,
                columns: vec!["id".into()],
            });
            def.indexes.push(unique_code);
        }
        "uq" => {
            def.columns = vec![ColumnDef::new("id", DataType::INT64), code, total];
            def.indexes.push(unique_code);
        }
        "nokey" => {
            def.columns = vec![
                ColumnDef::new("id", DataType::INT64),
                ColumnDef::new("code", DataType::Text),
                total,
            ];
            def.indexes.push(unique_code);
        }
        "extra" => {
            def.columns = vec![
                id,
                code,
                total,
                ColumnDef::new("missing", DataType::INT64).not_null(),
            ];
            def.primary_key = Some(PrimaryKeyDef {
                name: None,
                columns: vec!["missing".into()],
            });
        }
        _ => return None,
    }
    Some(def)
}

#[async_trait]
impl Fetcher for Tables {
    async fn list(
        &self,
        _c: &dyn Connection,
        parent: &ObjectRef,
        _cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        Ok(vec![ObjectSummary::new(
            parent.child(ObjectKind::Table, "t"),
        )])
    }

    async fn load(
        &self,
        _c: &dyn Connection,
        obj: &ObjectRef,
        _cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        if obj.kind == ObjectKind::View && obj.name == "v" {
            return Ok(ObjectDef::View(ViewDef {
                obj: obj.clone(),
                definition: "SELECT id, code FROM t".into(),
                columns: Vec::new(),
                comment: None,
                extras: Extras::new(),
            }));
        }
        table_def(&obj.name)
            .filter(|_| obj.kind == ObjectKind::Table)
            .map(ObjectDef::Table)
            .ok_or_else(|| Error::not_found(format!("no table {}", obj.name)))
    }
}

/// Writes with PostgreSQL quoting; reads back with `READBACK <key>` (an insert's key is 100),
/// or with `RETURNING *` when `returning`.
struct ToyWriter {
    returning: bool,
}

impl DmlWriter for ToyWriter {
    fn plan(&self, edit: &RowEdit, target: &DmlTarget<'_>) -> Result<DmlPlan> {
        let quoting = PgQuoting;
        let mut w = SqlWriter::new(&quoting);
        let key_column = target.key.columns[0].clone();
        let read_key = match edit {
            RowEdit::Insert { values } => {
                w.push("INSERT INTO ")
                    .qualified(&target.table.obj)
                    .push(" (");
                for (pos, v) in values.iter().enumerate() {
                    if pos > 0 {
                        w.push(", ");
                    }
                    w.ident(&v.column);
                }
                w.push(") VALUES (");
                for (pos, v) in values.iter().enumerate() {
                    if pos > 0 {
                        w.push(", ");
                    }
                    w.param(v.value.clone());
                }
                w.push(")");
                values
                    .iter()
                    .find(|v| v.column == key_column)
                    .map_or(Value::Int(100), |v| v.value.clone())
            }
            RowEdit::Update { key, changes } => {
                w.push("UPDATE ").qualified(&target.table.obj).push(" SET ");
                for (pos, c) in changes.iter().enumerate() {
                    if pos > 0 {
                        w.push(", ");
                    }
                    w.ident(&c.column).push(" = ").param(c.value.clone());
                }
                w.push(" WHERE ");
                write_key_predicate(&mut w, target.key, key)?;
                changes
                    .iter()
                    .find(|c| c.column == key_column)
                    .map_or_else(|| key[0].clone(), |c| c.value.clone())
            }
            RowEdit::Delete { key } => {
                w.push("DELETE FROM ")
                    .qualified(&target.table.obj)
                    .push(" WHERE ");
                write_key_predicate(&mut w, target.key, key)?;
                return Ok(DmlPlan::write_only(w.finish()));
            }
        };
        if self.returning {
            w.push(" RETURNING *");
            return Ok(DmlPlan {
                write: w.finish(),
                returning: true,
                read_back: None,
            });
        }
        Ok(DmlPlan {
            write: w.finish(),
            returning: false,
            read_back: Some(BoundStatement::new("READBACK", vec![read_key])),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

struct Harness {
    edit: EditService,
    query: Arc<QueryService>,
    state: Shared,
    session: SessionId,
    repo: Arc<MemoryHistoryRepository>,
    _spill: tempfile::TempDir,
}

/// `writer`: register a DML writer (`Some(returning)`), or none.
async fn harness(writer: Option<bool>) -> Harness {
    let state = Shared::default();
    let mut drivers = DriverRegistry::new();
    drivers.register(Arc::new(TableDriver {
        state: Arc::clone(&state),
    }));
    let manager = Arc::new(ConnectionManager::new(
        drivers,
        Arc::new(MemorySecretStore::new()),
        Arc::new(RusshTunnelProvider::new()),
    ));
    let mut profile = ConnectionProfile::new(
        "tables",
        Engine::Postgres,
        ProfileAddress::Network {
            host: "tables.invalid".into(),
            port: Some(5432),
        },
    );
    profile.database = Some("db0".into());
    let base = manager
        .open(&profile, &ProvidedSecrets::none(), CancellationToken::new())
        .await
        .unwrap();
    let mut dialects = DialectRegistry::new();
    dialects.register_fetcher(Engine::Postgres, ObjectKind::Table, Arc::new(Tables));
    dialects.register_fetcher(Engine::Postgres, ObjectKind::View, Arc::new(Tables));
    dialects.register_quoting(Engine::Postgres, Arc::new(PgQuoting));
    if let Some(returning) = writer {
        dialects.register_dml_writer(Engine::Postgres, Arc::new(ToyWriter { returning }));
    }
    let dialects = Arc::new(dialects);
    let spill = tempfile::tempdir().unwrap();
    let repo = Arc::new(MemoryHistoryRepository::new());
    let query = Arc::new(QueryService::new(
        Arc::clone(&manager),
        Arc::clone(&dialects),
        Arc::new(HistoryService::new(repo.clone())),
        QueryConfig {
            spill_dir: spill.path().join("results"),
            rows_event_interval: Duration::ZERO,
            ..QueryConfig::default()
        },
    ));
    let tree = Arc::new(TreeService::new(
        Arc::clone(&manager),
        Arc::clone(&dialects),
    ));
    let edit = EditService::new(Arc::clone(&query), tree, dialects);
    Harness {
        edit,
        query,
        state,
        session: base.id(),
        repo,
        _spill: spill,
    }
}

async fn drain(mut events: mpsc::Receiver<RunEvent>) -> Vec<RunEvent> {
    let mut out = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("the run must not stall")
            .expect("the run must end with a terminal event");
        let terminal = event.is_terminal();
        out.push(event);
        if terminal {
            return out;
        }
    }
}

fn table(name: &str) -> ObjectRef {
    ObjectRef::table(Some("db1"), Some("public"), name)
}

impl Harness {
    fn target(&self, editor: &str) -> RunTarget {
        RunTarget {
            session: self.session,
            editor: EditorId::new(editor),
        }
    }

    /// Opens `request` in editor `editor` and waits for its rows.
    async fn open_query(&self, editor: &str, request: TableDataQuery) -> RunId {
        let handle = self
            .edit
            .open_table_data(
                self.target(editor),
                request,
                RunOptions::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let events = drain(handle.events).await;
        assert!(
            matches!(events.last(), Some(RunEvent::Finished { .. })),
            "{events:?}"
        );
        handle.id
    }

    async fn open(&self, name: &str) -> RunId {
        self.open_query(name, TableDataQuery::new(table(name)))
            .await
    }

    async fn apply(&self, run: RunId, edits: Vec<PendingEdit>) -> ApplyReport {
        self.edit
            .apply(run, &PendingChanges { edits }, CancellationToken::new())
            .await
            .unwrap()
    }

    /// Log lines from now on.
    fn mark(&self) -> usize {
        lock(&self.state).log.len()
    }

    fn log_since(&self, mark: usize) -> Vec<String> {
        lock(&self.state).log[mark..].to_vec()
    }
}

fn update(row: u64, column: &str, text: &str) -> PendingEdit {
    PendingEdit::Update {
        row,
        cells: vec![CellEdit {
            column: column.into(),
            input: CellInput::text(text),
        }],
    }
}

fn insert(cells: &[(&str, CellInput)]) -> PendingEdit {
    PendingEdit::Insert {
        cells: cells
            .iter()
            .map(|(column, input)| CellEdit {
                column: (*column).to_owned(),
                input: input.clone(),
            })
            .collect(),
    }
}

fn statuses(report: &ApplyReport) -> Vec<EditStatus> {
    report.outcomes.iter().map(|o| o.status).collect()
}

fn texts(values: &[&str]) -> Vec<Option<String>> {
    values.iter().map(|v| Some((*v).to_owned())).collect()
}

// ---------------------------------------------------------------------------------------------
// Table data
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn open_table_data_builds_a_bound_select_on_the_table_database() {
    let h = harness(Some(false)).await;
    let mut request = TableDataQuery::new(table("t"));
    request.filter = Some(Filter::and(vec![
        Filter::condition("code", FilterOp::Eq, vec![Value::Text("a'; --".into())]),
        // Typed as text by the UI, bound as the column's type.
        Filter::condition("id", FilterOp::Gt, vec![Value::Text("1".into())]),
        Filter::condition("code", FilterOp::Contains, vec![Value::Text("5%".into())]),
    ]));
    request.sort = vec![SortKey::desc("id")];
    request.limit = Some(50);
    let mark = h.mark();
    let run = h.open_query("e1", request).await;
    assert_eq!(
        h.log_since(mark),
        [
            "connect db1",
            "SELECT * FROM \"public\".\"t\" WHERE (\"code\" = $1 AND \"id\" > $2 AND \"code\" \
             LIKE $3 ESCAPE '!') ORDER BY \"id\" DESC LIMIT 50 [a'; --, 1, %5!%%]"
        ]
    );
    // The value of `id > ?` is an integer now.
    let info = h.query.info(run).unwrap();
    assert_eq!(info.results[0].rows, 3);
    assert_eq!(info.statements.len(), 1);
    // Table data records no history.
    assert_eq!(
        h.repo.list(&HistoryQuery::default()).await.unwrap().len(),
        0
    );

    // Bad requests fail before anything runs.
    let mark = h.mark();
    for (request, expected) in [
        (
            {
                let mut r = TableDataQuery::new(table("t"));
                r.sort = vec![SortKey::asc("nope")];
                r
            },
            ErrorCategory::InvalidInput,
        ),
        (
            {
                let mut r = TableDataQuery::new(table("t"));
                r.filter = Some(Filter::condition(
                    "id",
                    FilterOp::Eq,
                    vec![Value::Text("one".into())],
                ));
                r
            },
            ErrorCategory::InvalidInput,
        ),
        (
            TableDataQuery::new(ObjectRef::new(ObjectKind::Function, "f")),
            ErrorCategory::InvalidInput,
        ),
        (TableDataQuery::new(table("gone")), ErrorCategory::NotFound),
    ] {
        let err = h
            .edit
            .open_table_data(
                h.target("e2"),
                request,
                RunOptions::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.category(), expected, "{err}");
    }
    assert_eq!(h.log_since(mark), Vec::<String>::new());
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let err = h
        .edit
        .open_table_data(
            h.target("e2"),
            TableDataQuery::new(table("t")),
            RunOptions::default(),
            cancelled,
        )
        .await
        .unwrap_err();
    assert!(err.is_cancelled());
}

#[tokio::test]
async fn editability_and_every_read_only_reason() {
    let h = harness(Some(false)).await;
    let run = h.open("t").await;
    let e = h.edit.begin(run, CancellationToken::new()).await.unwrap();
    assert!(e.editable, "{e:?}");
    assert_eq!(e.read_only_reason, None);
    assert_eq!(e.table, table("t"));
    let key = e.key.clone().unwrap();
    assert_eq!(
        (key.kind, key.columns),
        (KeyKind::Primary, vec!["id".to_owned()])
    );
    let columns: Vec<(&str, bool, bool, bool, Option<&str>)> = e
        .columns
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.key,
                c.editable,
                c.has_default,
                c.read_only_reason.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        columns,
        [
            ("id", true, true, true, None),
            ("code", false, true, false, None),
            ("total", false, false, true, Some("generated column")),
        ]
    );
    assert_eq!(e.columns[1].input, InputKind::Text);

    let uq = h.open("uq").await;
    let e = h.edit.begin(uq, CancellationToken::new()).await.unwrap();
    assert!(e.editable);
    let key = e.key.unwrap();
    assert_eq!(
        (key.kind, key.columns),
        (KeyKind::Unique, vec!["code".to_owned()])
    );

    for (name, reason) in [
        ("nokey", reasons::NO_KEY),
        ("extra", reasons::KEY_NOT_IN_RESULT),
    ] {
        let run = h.open(name).await;
        let e = h.edit.begin(run, CancellationToken::new()).await.unwrap();
        assert!(!e.editable, "{name}");
        assert_eq!(e.read_only_reason.as_deref(), Some(reason), "{name}");
        assert!(e.columns.iter().all(|c| !c.editable), "{name}");
    }

    let view = h
        .open_query(
            "v",
            TableDataQuery::new(ObjectRef::new(ObjectKind::View, "v")),
        )
        .await;
    let e = h.edit.begin(view, CancellationToken::new()).await.unwrap();
    assert_eq!(e.read_only_reason.as_deref(), Some(reasons::VIEW));
    assert_eq!(e.columns.len(), 3);

    let handle = h
        .query
        .run(h.target("q"), "SELECT 1", RunOptions::default())
        .await
        .unwrap();
    let query_run = handle.id;
    drain(handle.events).await;
    let e = h
        .edit
        .begin(query_run, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(e.read_only_reason.as_deref(), Some(reasons::NOT_TABLE_DATA));
    assert_eq!(e.columns.len(), 3);
    let err = h
        .edit
        .apply(query_run, &PendingChanges::new(), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);

    let err = h
        .edit
        .begin(RunId::new(), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);

    // An engine without a DML writer is read-only.
    let h = harness(None).await;
    let run = h.open("t").await;
    let e = h.edit.begin(run, CancellationToken::new()).await.unwrap();
    assert_eq!(e.read_only_reason.as_deref(), Some(reasons::ENGINE));
    let err = h
        .edit
        .apply(run, &PendingChanges::new(), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
}

// ---------------------------------------------------------------------------------------------
// Apply
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn invalid_edits_fail_before_anything_runs() {
    let h = harness(Some(false)).await;
    let run = h.open("t").await;
    let mark = h.mark();
    let report = h
        .apply(
            run,
            vec![
                update(0, "code", "x"),
                update(1, "total", "5"),
                insert(&[("code", CellInput::Null)]),
                insert(&[]),
                update(9, "code", "q"),
                update(0, "code", "again"),
                update(1, "id", "not a number"),
                PendingEdit::Update {
                    row: 2,
                    cells: vec![CellEdit {
                        column: "code".into(),
                        input: CellInput::Default,
                    }],
                },
                update(2, "nope", "1"),
            ],
        )
        .await;
    assert!(!report.committed);
    let mut expected = vec![EditStatus::Failed; 9];
    expected[0] = EditStatus::NotRun;
    assert_eq!(statuses(&report), expected);
    let messages: Vec<String> = report
        .outcomes
        .iter()
        .map(|o| {
            o.error
                .as_ref()
                .map(|e| e.message.clone())
                .unwrap_or_default()
        })
        .collect();
    for (index, expected) in [
        (1, "column 'total' is read-only (generated column)"),
        (2, "column 'code' is NOT NULL"),
        (3, "column 'code' is NOT NULL and has no default"),
        (4, "there is no row 9"),
        (5, "row 0 has more than one pending change"),
        (6, "column 'id': 'not a number' is not an integer"),
        (7, "DEFAULT can only be used for new rows"),
        (8, "the table has no column 'nope'"),
    ] {
        assert!(
            messages[index].contains(expected),
            "{index}: {}",
            messages[index]
        );
    }
    assert_eq!(report.outcomes[4].row, Some(9));
    // Nothing reached the database: not even a transaction.
    assert_eq!(h.log_since(mark), Vec::<String>::new());
}

#[tokio::test]
async fn edits_apply_in_one_transaction_with_read_back_and_the_overlay() {
    let h = harness(Some(false)).await;
    let run = h.open("t").await;
    let mark = h.mark();
    let report = h
        .apply(
            run,
            vec![
                update(0, "code", "z"),
                insert(&[("code", CellInput::text("new")), ("id", CellInput::Default)]),
                PendingEdit::Delete { row: 2 },
            ],
        )
        .await;
    assert!(report.committed, "{report:?}");
    assert_eq!(
        h.log_since(mark),
        [
            "BEGIN",
            "UPDATE \"public\".\"t\" SET \"code\" = $1 WHERE \"id\" = $2 [z, 1]",
            "READBACK [1]",
            "INSERT INTO \"public\".\"t\" (\"code\") VALUES ($1) [new]",
            "READBACK [100]",
            "DELETE FROM \"public\".\"t\" WHERE \"id\" = $1 [3]",
            "COMMIT",
        ]
    );
    let applied = |row: u64, values: Option<Vec<Option<String>>>| EditOutcome {
        status: EditStatus::Applied,
        error: None,
        row: Some(row),
        values,
        warning: None,
    };
    assert_eq!(
        report.outcomes,
        [
            applied(0, Some(texts(&["1", "read", "42"]))),
            // The new row is numbered after the 3 stored rows.
            applied(3, Some(texts(&["100", "read", "42"]))),
            applied(2, None),
        ]
    );

    // Forms show the values read back; the deleted row is gone.
    let form = h.edit.row_form(run, 0, 0).await.unwrap();
    assert_eq!(
        form.fields
            .iter()
            .map(|f| (f.column.as_str(), f.text.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("id", Some("1")),
            ("code", Some("read")),
            ("total", Some("42"))
        ]
    );
    assert_eq!(
        form.json,
        "{\n  \"id\": 1,\n  \"code\": \"read\",\n  \"total\": 42\n}"
    );
    let err = h.edit.row_form(run, 0, 2).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);
    assert_eq!(
        h.edit.row_form(run, 0, 3).await.unwrap().fields[0]
            .text
            .as_deref(),
        Some("100")
    );
    // An untouched stored row comes from the result store.
    let form = h.edit.row_form(run, 0, 1).await.unwrap();
    assert_eq!(form.fields[1].text.as_deref(), Some("b"));
    assert!(h.edit.row_form(run, 0, 4).await.is_err());
    assert!(h.edit.row_form(run, 1, 0).await.is_err());

    // Later edits use the overlay: the new row's key, the deleted row refused.
    let mark = h.mark();
    let report = h
        .apply(
            run,
            vec![update(3, "code", "again"), PendingEdit::Delete { row: 2 }],
        )
        .await;
    assert_eq!(statuses(&report), [EditStatus::NotRun, EditStatus::Failed]);
    assert!(
        report.outcomes[1]
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("row 2 was deleted")
    );
    assert_eq!(h.log_since(mark), Vec::<String>::new());
    let report = h.apply(run, vec![update(3, "code", "again")]).await;
    assert!(report.committed);
    assert_eq!(
        h.log_since(mark),
        [
            "BEGIN",
            "UPDATE \"public\".\"t\" SET \"code\" = $1 WHERE \"id\" = $2 [again, 100]",
            "READBACK [100]",
            "COMMIT",
        ]
    );
    // An empty buffer commits nothing.
    let report = h.apply(run, Vec::new()).await;
    assert!(report.committed && report.outcomes.is_empty());

    // Closing the run forgets everything.
    h.query.close(run).await.unwrap();
    let err = h.edit.row_form(run, 0, 0).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);
    let err = h
        .edit
        .apply(run, &PendingChanges::new(), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);
}

#[tokio::test]
async fn a_conflict_rolls_the_whole_apply_back() {
    let h = harness(Some(false)).await;
    let run = h.open("t").await;
    // The second update matches no row (someone else deleted it).
    lock(&h.state).affected.extend([1, 0]);
    let mark = h.mark();
    let report = h
        .apply(
            run,
            vec![
                update(0, "code", "x"),
                update(1, "code", "y"),
                PendingEdit::Delete { row: 2 },
            ],
        )
        .await;
    assert!(!report.committed);
    assert_eq!(
        statuses(&report),
        [
            EditStatus::RolledBack,
            EditStatus::Conflict,
            EditStatus::NotRun
        ]
    );
    let error = report.outcomes[1].error.as_ref().unwrap();
    assert!(
        error.message.contains("changed or deleted by someone else"),
        "{error:?}"
    );
    let log = h.log_since(mark);
    assert_eq!(log.first().map(String::as_str), Some("BEGIN"));
    assert_eq!(log.last().map(String::as_str), Some("ROLLBACK"));
    assert!(!log.iter().any(|l| l == "COMMIT"));
    assert!(!log.iter().any(|l| l.starts_with("DELETE")));

    // Two rows matched: also a conflict.
    lock(&h.state).affected.push_back(2);
    let report = h.apply(run, vec![PendingEdit::Delete { row: 2 }]).await;
    assert_eq!(statuses(&report), [EditStatus::Conflict]);
    assert!(
        report.outcomes[0]
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("2 rows matched")
    );
    // Nothing was kept: the rows can still be edited.
    let report = h.apply(run, vec![PendingEdit::Delete { row: 2 }]).await;
    assert!(report.committed);
}

#[tokio::test]
async fn a_server_error_fails_its_edit_and_rolls_back() {
    let h = harness(Some(false)).await;
    let run = h.open("t").await;
    let mark = h.mark();
    let report = h
        .apply(
            run,
            vec![update(0, "code", "ok"), update(1, "code", "FAIL")],
        )
        .await;
    assert_eq!(
        statuses(&report),
        [EditStatus::RolledBack, EditStatus::Failed]
    );
    let error = report.outcomes[1].error.as_ref().unwrap();
    assert_eq!(error.category, ErrorCategory::Sql);
    assert_eq!(
        h.log_since(mark).last().map(String::as_str),
        Some("ROLLBACK")
    );
}

#[tokio::test]
async fn returning_writers_count_returned_rows() {
    let h = harness(Some(true)).await;
    let run = h.open("t").await;
    let report = h.apply(run, vec![update(0, "code", "x")]).await;
    assert!(report.committed);
    assert_eq!(report.outcomes[0].values, Some(texts(&["7", "ret", "14"])));
    lock(&h.state).affected.push_back(2);
    let report = h.apply(run, vec![update(1, "code", "x")]).await;
    assert_eq!(statuses(&report), [EditStatus::Conflict]);
    lock(&h.state).affected.push_back(0);
    let report = h.apply(run, vec![update(1, "code", "x")]).await;
    assert_eq!(statuses(&report), [EditStatus::Conflict]);
}

#[tokio::test]
async fn apply_is_refused_while_the_rows_are_loading() {
    let h = harness(Some(false)).await;
    let handle = h
        .edit
        .open_table_data(
            h.target("slow"),
            TableDataQuery::new(table("slow")),
            RunOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let err = h
        .edit
        .apply(
            handle.id,
            &PendingChanges {
                edits: vec![update(0, "code", "x")],
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert!(err.to_string().contains("still loading"), "{err}");
    assert!(h.query.cancel(handle.id));
    let events = drain(handle.events).await;
    assert!(matches!(events.last(), Some(RunEvent::Cancelled { .. })));
}

#[tokio::test]
async fn forms_of_query_results_are_read_only_rows() {
    let h = harness(Some(false)).await;
    let handle = h
        .query
        .run(h.target("q"), "SELECT x", RunOptions::default())
        .await
        .unwrap();
    let run = handle.id;
    drain(handle.events).await;
    let form = h.edit.row_form(run, 0, 2).await.unwrap();
    assert_eq!(form.row, 2);
    assert_eq!(
        form.fields
            .iter()
            .map(|f| f.text.as_deref())
            .collect::<Vec<_>>(),
        [Some("3"), Some("c"), None]
    );
    assert_eq!(
        form.json,
        "{\n  \"id\": 3,\n  \"code\": \"c\",\n  \"total\": null\n}"
    );
    assert!(h.edit.row_form(RunId::new(), 0, 0).await.is_err());
}

// ---------------------------------------------------------------------------------------------
// QueryService::run_bound / run_session
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn run_bound_is_one_statement_with_bound_values_and_no_values_in_history() {
    let h = harness(Some(false)).await;
    let sql = "SELECT * FROM t WHERE a = $1;\n-- not split; nothing after";
    let mark = h.mark();
    let handle = h
        .query
        .run_bound(
            h.target("b"),
            BoundStatement::new(sql, vec![Value::Text("hunter2".into())]),
            RunOptions::default(),
        )
        .await
        .unwrap();
    let run = handle.id;
    let events = drain(handle.events).await;
    let RunEvent::Started { statements, .. } = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(statements.len(), 1);
    assert_eq!(
        (statements[0].start_byte, statements[0].end_byte),
        (0, sql.len())
    );
    assert!(statements[0].returns_rows);
    assert!(matches!(events.last(), Some(RunEvent::Finished { .. })));
    // The driver got the whole text and the value as a parameter.
    assert_eq!(h.log_since(mark)[1], format!("{sql} [hunter2]"));
    // Paging is the M2 path.
    let page = h.query.page(run, 0, 1, 10).await.unwrap();
    assert_eq!(page.num_rows(), 2);
    // History has the SQL text, never the value.
    let entries = h.repo.list(&HistoryQuery::default()).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert!(!entries[0].sql.contains("hunter2"));
    assert!(entries[0].sql.starts_with("SELECT * FROM t WHERE a = $1"));

    // The run's session is the editor's child session on its database.
    let session = h.query.run_session(run).unwrap();
    assert_eq!(session.summary().parent, Some(h.session));
    assert_eq!(h.query.run_state(run), Some(RunState::Finished));
    h.query.close(run).await.unwrap();
    assert_eq!(h.query.run_state(run), None);
    let err = h.query.run_session(run).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);

    // A closed editor session is reported too.
    let handle = h
        .query
        .run_bound(
            h.target("c"),
            BoundStatement::new("SELECT 1", Vec::new()),
            RunOptions::default(),
        )
        .await
        .unwrap();
    let run = handle.id;
    drain(handle.events).await;
    let session = h.query.run_session(run).unwrap();
    h.query.manager().close(session.id()).await.unwrap();
    assert_eq!(
        h.query.run_session(run).unwrap_err().category(),
        ErrorCategory::NotFound
    );
}
