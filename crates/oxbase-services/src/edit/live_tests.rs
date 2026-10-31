//! Live tests of table data and grid editing against the Docker fixtures (PostgreSQL, MySQL,
//! MariaDB) and a SQLite file, plus the live harness the designer's live tests share.
//!
//! Each test works on its own scratch table `m3_svc_<token>_<n>` (created and dropped here), so
//! it does not depend on the engine streams' seed scripts:
//!
//! - open table data with a filter (text values converted to the column type) and a sort;
//! - editability: the primary key, the generated column read-only;
//! - update / insert / delete through `EditService` in one transaction, with read-back of the
//!   generated column;
//! - a row deleted by another session makes the edit a conflict and rolls the batch back;
//! - the `[m3]` seed tables, when the engine's init script created them: `m3_edit_uq` keyed by
//!   `code`, `m3_edit_nokey` and `m3_edit_view` read-only.
//!
//! Grid edits need the engine's `DmlWriter` (engine streams). Until an engine registers one,
//! these tests register a generic writer for it (plain `INSERT` / `UPDATE ... WHERE key` /
//! `DELETE`, read back by key with a query), so the service's transaction, conflict and overlay
//! logic still runs live; with the engine's own writer they test that writer too.
//!
//! ```sh
//! scripts/fixtures-up.sh
//! cargo test -p oxbase-services --lib edit::live_tests -- --ignored --test-threads 1
//! ```

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use oxbase_dialect::sql::SqlWriter;
use oxbase_dialect::{Quoting, write_column_list, write_key_predicate};
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_store::{
    ConnectionProfile, MemoryHistoryRepository, MemorySecretStore, ProfileAddress, Secret,
    SecretRef, SecretSlot, SecretStore, TlsMode, TlsSettings,
};
use oxbase_types::{FilterOp, SortKey};
use tokio::sync::mpsc;

use super::*;
use crate::designer::DesignerService;
use crate::history::HistoryService;
use crate::query::{EditorId, QueryConfig, RunEvent};
use crate::{ConnectionManager, ProvidedSecrets, SessionId, default_dialects, default_drivers};

/// Writes a line to stderr, bypassing the test harness's output capture.
pub(crate) fn report(line: &str) {
    let _ = writeln!(std::io::stderr().lock(), "[m3 services] {line}");
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn fixtures() -> toml::Table {
    let path = std::env::var_os("OXBASE_FIXTURES")
        .map(PathBuf::from)
        .map_or_else(|| root().join("tests/fixtures.toml"), |p| root().join(p));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.parse().expect("fixtures.toml parses")
}

fn table_of<'a>(t: &'a toml::Table, key: &str) -> &'a toml::Table {
    t.get(key)
        .and_then(toml::Value::as_table)
        .unwrap_or_else(|| panic!("fixtures.toml: missing table {key}"))
}

fn string(t: &toml::Table, key: &str) -> String {
    t.get(key)
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("fixtures.toml: missing string {key}"))
        .to_owned()
}

/// The engines the M3 live tests run on (`[engines.<key>]`).
pub(crate) const ENGINES: [(&str, Engine); 4] = [
    ("postgres", Engine::Postgres),
    ("mysql", Engine::MySql),
    ("mariadb", Engine::MariaDb),
    ("sqlite", Engine::Sqlite),
];

/// 10 lowercase base36 characters from the time and the process id (unique per run).
pub(crate) fn token() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    let mut n =
        (u128::from(secs) << 32) | (u128::from(std::process::id()) << 8) | u128::from(nanos % 256);
    let mut out = String::new();
    while out.len() < 10 {
        let digit = u32::try_from(n % 36).unwrap_or_default();
        out.push(char::from_digit(digit, 36).unwrap_or('0'));
        n /= 36;
    }
    out
}

/// A generic DML writer for engines that have not registered theirs yet: bound values,
/// key-only matches, read-back by key (none after an insert whose key the engine generated).
struct GenericWriter {
    quoting: Arc<dyn Quoting>,
}

impl DmlWriter for GenericWriter {
    fn plan(&self, edit: &RowEdit, target: &DmlTarget<'_>) -> Result<DmlPlan> {
        let mut w = SqlWriter::new(self.quoting.as_ref());
        let read_key: Option<Vec<Value>> = match edit {
            RowEdit::Insert { values } => {
                w.push("INSERT INTO ").qualified(&target.table.obj);
                if values.is_empty() {
                    w.push(" DEFAULT VALUES");
                } else {
                    let columns: Vec<String> = values.iter().map(|v| v.column.clone()).collect();
                    w.push(" (");
                    write_column_list(&mut w, &columns);
                    w.push(") VALUES (");
                    for (pos, v) in values.iter().enumerate() {
                        if pos > 0 {
                            w.push(", ");
                        }
                        w.param(v.value.clone());
                    }
                    w.push(")");
                }
                target
                    .key
                    .columns
                    .iter()
                    .map(|k| {
                        values
                            .iter()
                            .find(|v| v.column == *k)
                            .map(|v| v.value.clone())
                    })
                    .collect()
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
                Some(
                    target
                        .key
                        .columns
                        .iter()
                        .zip(key)
                        .map(|(k, old)| {
                            changes
                                .iter()
                                .find(|c| c.column == *k)
                                .map_or_else(|| old.clone(), |c| c.value.clone())
                        })
                        .collect(),
                )
            }
            RowEdit::Delete { key } => {
                w.push("DELETE FROM ")
                    .qualified(&target.table.obj)
                    .push(" WHERE ");
                write_key_predicate(&mut w, target.key, key)?;
                return Ok(DmlPlan::write_only(w.finish()));
            }
        };
        let read_back = match read_key {
            Some(values) if !target.read_back.is_empty() => {
                let mut r = SqlWriter::new(self.quoting.as_ref());
                r.push("SELECT ");
                write_column_list(&mut r, target.read_back);
                r.push(" FROM ")
                    .qualified(&target.table.obj)
                    .push(" WHERE ");
                write_key_predicate(&mut r, target.key, &values)?;
                Some(r.finish())
            }
            _ => None,
        };
        Ok(DmlPlan {
            write: w.finish(),
            returning: false,
            read_back,
        })
    }
}

/// A connected base session on one engine with every M3 service.
pub(crate) struct Live {
    pub engine: Engine,
    pub key: &'static str,
    pub manager: Arc<ConnectionManager>,
    pub dialects: Arc<DialectRegistry>,
    pub query: Arc<QueryService>,
    pub edit: EditService,
    pub designer: DesignerService,
    pub session: SessionId,
    /// The database the scratch tables go into.
    pub catalog: String,
    /// Their schema (PostgreSQL `public`).
    pub schema: Option<String>,
    pub token: String,
    _dirs: tempfile::TempDir,
}

impl Live {
    /// Connects to engine `key`; with `generic_writer`, an engine without a `DmlWriter` gets
    /// the generic one.
    pub(crate) async fn connect(key: &'static str, engine: Engine, generic_writer: bool) -> Self {
        let fx = fixtures();
        let dirs = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemorySecretStore::new());
        let (profile, catalog, schema) = if engine == Engine::Sqlite {
            let profile = ConnectionProfile::new(
                format!("live {key}"),
                engine,
                ProfileAddress::File {
                    path: dirs.path().join("live.sqlite"),
                },
            );
            (profile, "main".to_owned(), None)
        } else {
            let e = table_of(table_of(&fx, "engines"), key);
            let mut p = ConnectionProfile::new(
                format!("live {key}"),
                engine,
                ProfileAddress::Network {
                    host: string(e, "host"),
                    port: e
                        .get("port")
                        .and_then(toml::Value::as_integer)
                        .and_then(|p| u16::try_from(p).ok()),
                },
            );
            p.user = Some(string(e, "user"));
            let database = string(e, "database");
            p.database = Some(database.clone());
            let secret = p.secret_key(SecretSlot::Password);
            secrets
                .set(&secret, &Secret::new(string(e, "password")))
                .await
                .unwrap();
            p.password = Some(SecretRef::Keychain { key: secret });
            p.tls = TlsSettings {
                mode: TlsMode::Disable,
                ..TlsSettings::default()
            };
            p.connect_timeout_secs = Some(15);
            let schema = (engine == Engine::Postgres).then(|| "public".to_owned());
            (p, database, schema)
        };
        let manager = Arc::new(ConnectionManager::new(
            default_drivers(),
            secrets,
            Arc::new(RusshTunnelProvider::new()),
        ));
        let base = manager
            .open(&profile, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{key}: connect failed: {e}"));
        let mut registry = default_dialects();
        if generic_writer
            && registry.dml_writer(engine).is_err()
            && let Ok(quoting) = registry.quoting(engine)
        {
            report(&format!(
                "{key}: no DmlWriter registered yet; using the generic test writer"
            ));
            registry.register_dml_writer(engine, Arc::new(GenericWriter { quoting }));
        }
        let dialects = Arc::new(registry);
        let query = Arc::new(QueryService::new(
            Arc::clone(&manager),
            Arc::clone(&dialects),
            Arc::new(HistoryService::new(
                Arc::new(MemoryHistoryRepository::new()),
            )),
            QueryConfig {
                spill_dir: dirs.path().join("results"),
                ..QueryConfig::default()
            },
        ));
        let tree = Arc::new(TreeService::new(
            Arc::clone(&manager),
            Arc::clone(&dialects),
        ));
        let edit = EditService::new(Arc::clone(&query), Arc::clone(&tree), Arc::clone(&dialects));
        let designer = DesignerService::new(Arc::clone(&manager), tree, Arc::clone(&dialects));
        Self {
            engine,
            key,
            manager,
            dialects,
            query,
            edit,
            designer,
            session: base.id(),
            catalog,
            schema,
            token: token(),
            _dirs: dirs,
        }
    }

    /// A table ref in the scratch database / schema.
    pub(crate) fn table(&self, name: &str) -> ObjectRef {
        ObjectRef::table(Some(&self.catalog), self.schema.as_deref(), name)
    }

    /// The quoted, qualified name of `table`.
    pub(crate) fn qualified(&self, table: &ObjectRef) -> String {
        self.dialects.quoting(self.engine).unwrap().qualify(table)
    }

    pub(crate) fn target(&self, editor: &str) -> RunTarget {
        RunTarget {
            session: self.session,
            editor: EditorId::new(editor),
        }
    }

    /// Runs `script` on the helper editor; panics when a statement fails.
    pub(crate) async fn sql(&self, script: &str) {
        let events = self.run(script).await.1;
        if let Some(failure) = events.iter().find_map(|e| match e {
            RunEvent::StatementFailed { error, .. } => Some(error.message.clone()),
            _ => None,
        }) {
            panic!("{}: {script}: {failure}", self.key);
        }
    }

    /// Runs `script` on the helper editor and returns the run and its events.
    pub(crate) async fn run(&self, script: &str) -> (RunId, Vec<RunEvent>) {
        let options = RunOptions {
            record_history: false,
            ..RunOptions::default()
        };
        let handle = self
            .query
            .run(self.target("live-sql"), script, options)
            .await
            .unwrap_or_else(|e| panic!("{}: {script}: {e}", self.key));
        (handle.id, drain(handle.events).await)
    }

    /// The rows of `select` as edit texts.
    pub(crate) async fn rows(&self, select: &str) -> Vec<Vec<Option<String>>> {
        let (run, events) = self.run(select).await;
        assert!(
            matches!(events.last(), Some(RunEvent::Finished { .. })),
            "{}: {select}: {events:?}",
            self.key
        );
        let page = self.query.page(run, 0, 0, 10_000).await.unwrap();
        (0..page.num_rows())
            .map(|row| {
                page.columns()
                    .iter()
                    .map(|c| cell_text(c.as_ref(), row).unwrap())
                    .collect()
            })
            .collect()
    }

    /// `true` when `table` exists (its definition loads).
    pub(crate) async fn exists(&self, table: &ObjectRef) -> bool {
        self.edit
            .tree()
            .load(self.session, table, CancellationToken::new())
            .await
            .is_ok()
    }

    /// Drops `table` if it exists (clean-up).
    pub(crate) async fn drop_table(&self, table: &ObjectRef) {
        let q = self.qualified(table);
        let _ = self.run(&format!("DROP TABLE IF EXISTS {q}")).await;
    }
}

pub(crate) async fn drain(mut events: mpsc::Receiver<RunEvent>) -> Vec<RunEvent> {
    let mut out = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(60), events.recv())
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

/// `CREATE TABLE` of the scratch table: a generated integer key, a NOT NULL unique code, a
/// decimal amount and a stored generated `total = amount * 2` (test fixture SQL per engine).
fn create_scratch(engine: Engine, qualified: &str) -> String {
    let (id, extra) = match engine {
        Engine::Postgres => (
            "id integer GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY",
            "",
        ),
        Engine::Sqlite => ("id INTEGER PRIMARY KEY", ""),
        _ => ("id int AUTO_INCREMENT PRIMARY KEY", " ENGINE=InnoDB"),
    };
    format!(
        "CREATE TABLE {qualified} ({id}, code varchar(20) NOT NULL UNIQUE, amount decimal(12,2), \
         total decimal(13,2) GENERATED ALWAYS AS (amount * 2) STORED){extra}"
    )
}

fn number(text: Option<&str>) -> Option<f64> {
    text.and_then(|t| t.parse().ok())
}

fn close(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-9)
}

async fn open(live: &Live, editor: &str, request: TableDataQuery) -> RunId {
    let handle = live
        .edit
        .open_table_data(
            live.target(editor),
            request,
            RunOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("{}: open_table_data: {e}", live.key));
    let events = drain(handle.events).await;
    assert!(
        matches!(events.last(), Some(RunEvent::Finished { .. })),
        "{}: {events:?}",
        live.key
    );
    handle.id
}

/// The codes and amounts of `run`'s rows, in result order.
async fn codes(live: &Live, run: RunId, code_index: usize) -> Vec<String> {
    let page = live.query.page(run, 0, 0, 1000).await.unwrap();
    (0..page.num_rows())
        .map(|row| {
            cell_text(page.column(code_index).as_ref(), row)
                .unwrap()
                .unwrap_or_default()
        })
        .collect()
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

async fn edit_cycle(key: &'static str, engine: Engine) {
    let live = Live::connect(key, engine, true).await;
    let table = live.table(&format!("m3_svc_{}_e", live.token));
    let q = live.qualified(&table);
    live.drop_table(&table).await;
    live.sql(&create_scratch(engine, &q)).await;
    live.sql(&format!(
        "INSERT INTO {q} (code, amount) VALUES ('a', 1.25), ('b', 2.50), ('c', NULL), ('d', 0.50)"
    ))
    .await;
    let result = edit_steps(&live, &table, &q).await;
    live.drop_table(&table).await;
    if let Err(message) = result {
        panic!("{key}: {message}");
    }
    report(&format!("{key}: table data, edits and conflicts ok"));
}

async fn edit_steps(live: &Live, table: &ObjectRef, q: &str) -> std::result::Result<(), String> {
    // Filtered (a text value converted to the decimal type) and sorted.
    let mut request = TableDataQuery::new(table.clone());
    request.filter = Some(Filter::condition(
        "amount",
        FilterOp::Ge,
        vec![Value::Text("1.25".into())],
    ));
    request.sort = vec![SortKey::desc("code")];
    let run = open(live, "grid", request).await;
    let e = live
        .edit
        .begin(run, CancellationToken::new())
        .await
        .map_err(|e| format!("begin: {e}"))?;
    let index = |name: &str| e.columns.iter().position(|c| c.name == name).unwrap();
    let (code, total, amount) = (index("code"), index("total"), index("amount"));
    if codes(live, run, code).await != ["b", "a"] {
        return Err(format!("filtered rows: {:?}", codes(live, run, code).await));
    }
    if !e.editable {
        return Err(format!("not editable: {:?}", e.read_only_reason));
    }
    let key = e.key.clone().unwrap();
    if key.columns != ["id"] || key.kind != KeyKind::Primary {
        return Err(format!("key: {key:?}"));
    }
    if e.columns[total].editable || e.columns[total].read_only_reason.is_none() {
        return Err(format!("generated column editable: {:?}", e.columns[total]));
    }
    if e.columns[amount].input != InputKind::Decimal {
        return Err(format!("amount input: {:?}", e.columns[amount].input));
    }

    // Update 'b', insert 'e', delete 'a' in one transaction.
    let report = live
        .edit
        .apply(
            run,
            &PendingChanges {
                edits: vec![
                    update(0, "amount", "10.50"),
                    PendingEdit::Insert {
                        cells: vec![
                            CellEdit {
                                column: "code".into(),
                                input: CellInput::text("e"),
                            },
                            CellEdit {
                                column: "amount".into(),
                                input: CellInput::text("3"),
                            },
                        ],
                    },
                    PendingEdit::Delete { row: 1 },
                ],
            },
            CancellationToken::new(),
        )
        .await
        .map_err(|e| format!("apply: {e}"))?;
    if !report.committed {
        return Err(format!("apply: {report:?}"));
    }
    let updated = report.outcomes[0]
        .values
        .clone()
        .ok_or("the update was not read back")?;
    if !close(number(updated[total].as_deref()), 21.0) {
        return Err(format!("read-back of the generated total: {updated:?}"));
    }
    if report.outcomes[1].row != Some(2) {
        return Err(format!("insert row: {:?}", report.outcomes[1]));
    }
    if let Some(inserted) = &report.outcomes[1].values {
        // The engine's writer reads the new row back, generated key and total included.
        if inserted[code].as_deref() != Some("e") || !close(number(inserted[total].as_deref()), 6.0)
        {
            return Err(format!("insert read-back: {inserted:?}"));
        }
        if inserted[index("id")].is_none() {
            return Err("the generated key was not read back".into());
        }
    }
    // The form shows the read-back values; the deleted row is gone.
    let form = live
        .edit
        .row_form(run, 0, 0)
        .await
        .map_err(|e| format!("form: {e}"))?;
    if !close(number(form.fields[total].text.as_deref()), 21.0) {
        return Err(format!("form: {form:?}"));
    }
    if live.edit.row_form(run, 0, 1).await.is_ok() {
        return Err("the deleted row still has a form".into());
    }
    let rows = live
        .rows(&format!("SELECT code, amount FROM {q} ORDER BY code"))
        .await;
    let found: Vec<(String, Option<f64>)> = rows
        .iter()
        .map(|r| (r[0].clone().unwrap_or_default(), number(r[1].as_deref())))
        .collect();
    let expected = [
        ("b".to_owned(), Some(10.5)),
        ("c".to_owned(), None),
        ("d".to_owned(), Some(0.5)),
        ("e".to_owned(), Some(3.0)),
    ];
    if found != expected {
        return Err(format!("rows after apply: {found:?}"));
    }

    // Another session deletes 'd' after it was loaded: the batch conflicts and rolls back.
    let mut request = TableDataQuery::new(table.clone());
    request.sort = vec![SortKey::asc("code")];
    let run = open(live, "grid2", request).await;
    if codes(live, run, code).await != ["b", "c", "d", "e"] {
        return Err(format!("all rows: {:?}", codes(live, run, code).await));
    }
    live.sql(&format!("DELETE FROM {q} WHERE code = 'd'")).await;
    let report = live
        .edit
        .apply(
            run,
            &PendingChanges {
                edits: vec![update(1, "amount", "7"), update(2, "amount", "8")],
            },
            CancellationToken::new(),
        )
        .await
        .map_err(|e| format!("apply: {e}"))?;
    let statuses: Vec<EditStatus> = report.outcomes.iter().map(|o| o.status).collect();
    if report.committed || statuses != [EditStatus::RolledBack, EditStatus::Conflict] {
        return Err(format!("conflict: {report:?}"));
    }
    let rows = live
        .rows(&format!("SELECT amount FROM {q} WHERE code = 'c'"))
        .await;
    if rows != [vec![None]] {
        return Err(format!("the rolled-back edit stayed: {rows:?}"));
    }
    Ok(())
}

/// The `[m3]` seed tables, when the engine's init script created them.
async fn seed_tables(key: &'static str, engine: Engine) {
    let live = Live::connect(key, engine, false).await;
    let fx = fixtures();
    let m3 = table_of(&fx, "m3");
    let engine_m3 = table_of(m3, key);
    if engine == Engine::Sqlite {
        let init = root().join(string(engine_m3, "init_sql"));
        let Ok(script) = std::fs::read_to_string(&init) else {
            report(&format!(
                "{key}: seed tables pending ({} missing)",
                init.display()
            ));
            return;
        };
        live.sql(&script).await;
    }
    let schema = engine_m3
        .get("schema")
        .and_then(toml::Value::as_str)
        .map(ToOwned::to_owned);
    let seed = |name: &str, kind: ObjectKind| {
        let mut r = ObjectRef::table(Some(&live.catalog), schema.as_deref(), name);
        r.kind = kind;
        r
    };
    let pk = seed(&string(m3, "edit_pk_table"), ObjectKind::Table);
    if !live.exists(&pk).await {
        report(&format!("{key}: seed tables pending (no {})", pk.name));
        return;
    }
    let has_writer = live.dialects.dml_writer(engine).is_ok();
    let begin = |table: ObjectRef, editor: &'static str| {
        let live = &live;
        async move {
            let run = open(live, editor, TableDataQuery::new(table)).await;
            live.edit
                .begin(run, CancellationToken::new())
                .await
                .unwrap()
        }
    };
    // m3_edit_pk: filtered and sorted; the generated `total` is a read-only cell.
    let mut request = TableDataQuery::new(pk.clone());
    request.filter = Some(Filter::condition(
        "code",
        FilterOp::In,
        vec![Value::Text("a".into()), Value::Text("b".into())],
    ));
    request.sort = vec![SortKey::desc("id")];
    let run = open(&live, "pk", request).await;
    let e = live
        .edit
        .begin(run, CancellationToken::new())
        .await
        .unwrap();
    let code = e.columns.iter().position(|c| c.name == "code").unwrap();
    assert_eq!(codes(&live, run, code).await, ["b", "a"], "{key}");
    let total = e.columns.iter().find(|c| c.name == "total").unwrap();
    assert!(!total.editable, "{key}: {total:?}");
    assert_eq!(e.editable, has_writer, "{key}: {e:?}");

    let e = begin(
        seed(&string(m3, "edit_unique_table"), ObjectKind::Table),
        "uq",
    )
    .await;
    assert_eq!(e.editable, has_writer, "{key}: {e:?}");
    let k = e.key.unwrap();
    assert_eq!(
        (k.kind, k.columns),
        (KeyKind::Unique, vec!["code".to_owned()]),
        "{key}"
    );
    let e = begin(
        seed(&string(m3, "edit_nokey_table"), ObjectKind::Table),
        "nokey",
    )
    .await;
    assert!(!e.editable, "{key}");
    if has_writer {
        assert_eq!(
            e.read_only_reason.as_deref(),
            Some(reasons::NO_KEY),
            "{key}"
        );
    }
    let e = begin(seed(&string(m3, "edit_view"), ObjectKind::View), "view").await;
    assert_eq!(e.read_only_reason.as_deref(), Some(reasons::VIEW), "{key}");
    report(&format!(
        "{key}: seed tables ok (engine writer: {has_writer})"
    ));
}

macro_rules! per_engine {
    ($($name:ident => $body:ident($key:literal, $engine:expr);)*) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            #[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
            async fn $name() {
                $body($key, $engine).await;
            }
        )*
    };
}

per_engine! {
    postgres_table_data_edits_and_conflicts => edit_cycle("postgres", Engine::Postgres);
    mysql_table_data_edits_and_conflicts => edit_cycle("mysql", Engine::MySql);
    mariadb_table_data_edits_and_conflicts => edit_cycle("mariadb", Engine::MariaDb);
    sqlite_table_data_edits_and_conflicts => edit_cycle("sqlite", Engine::Sqlite);
    postgres_seed_tables => seed_tables("postgres", Engine::Postgres);
    mysql_seed_tables => seed_tables("mysql", Engine::MySql);
    mariadb_seed_tables => seed_tables("mariadb", Engine::MariaDb);
    sqlite_seed_tables => seed_tables("sqlite", Engine::Sqlite);
}

#[test]
fn tokens_are_ten_base36_characters() {
    let t = token();
    assert_eq!(t.len(), 10);
    assert!(
        t.chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase())
    );
}
