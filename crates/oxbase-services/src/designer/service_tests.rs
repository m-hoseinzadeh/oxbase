//! `execute_script` and `DesignerService` with a scripted in-memory driver (no Docker): phases,
//! captures, conditions, `expect_no_rows`, finally after a failure, non-atomic stops,
//! cancellation; load, preview (toy generator, the shared comparator, reference checks),
//! apply (refusals, the conflict check, a dedicated session, reload).

use std::sync::Mutex;

use arrow_array::{ArrayRef, RecordBatch, StringArray};
use arrow_schema::{DataType as ArrowType, Field, Schema};
use async_trait::async_trait;
use futures::{StreamExt, stream};
use oxbase_designer::{DesignColumn, DesignItem, NOT_ATOMIC_NOTE};
use oxbase_dialect::compare::{DefaultRules, RulesComparator};
use oxbase_dialect::{Capabilities, DdlContext, DdlGenerator, Fetcher, PgQuoting, Support};
use oxbase_driver::{ConnectParams, Driver, DriverRegistry, ResultItem, ResultStream};
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_store::{ConnectionProfile, MemorySecretStore, ProfileAddress};
use oxbase_types::{
    ColumnDef, DataType, Extras, FkAction, ForeignKeyDef, ObjectSummary, PrimaryKeyDef, PropId,
    Value,
};
use pretty_assertions::assert_eq;

use super::*;
use crate::ProvidedSecrets;

// ---------------------------------------------------------------------------------------------
// Scripted driver and a toy "server"
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Server {
    /// Statements (`<database>: sql`), `BEGIN` / `COMMIT` / `ROLLBACK`, `connect <db>`,
    /// `close <db>`.
    log: Vec<String>,
    /// The tables the fetcher returns now.
    tables: Vec<TableDef>,
    /// What table `t` becomes once a DDL statement is committed.
    after: Option<TableDef>,
}

type Shared = Arc<Mutex<Server>>;

fn server(s: &Shared) -> std::sync::MutexGuard<'_, Server> {
    s.lock().unwrap()
}

/// Answers: `CAPTURE <v>` one row with `v` (`NULL`: a NULL); `ROWS <n>` `n` rows; `CANCEL`
/// cancels its own token (the caller's); anything containing `FAIL` or `boom` an SQL error;
/// anything else `RowsAffected(0)`. A DDL statement (`ALTER`, `CREATE`) turns table `t` into
/// `after` when it runs outside a transaction, or when its transaction commits.
struct ScriptDriver {
    server: Shared,
}

#[async_trait]
impl Driver for ScriptDriver {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        let database = params.database.clone().unwrap_or_else(|| "-".into());
        server(&self.server).log.push(format!("connect {database}"));
        Ok(Box::new(ScriptConnection {
            server: Arc::clone(&self.server),
            database,
            version: ServerVersion::new(17, 0, 0),
        }))
    }
}

struct ScriptConnection {
    server: Shared,
    database: String,
    version: ServerVersion,
}

fn rows(values: &[Option<&str>]) -> Vec<Result<ResultItem>> {
    let schema = Arc::new(Schema::new(vec![Field::new("n", ArrowType::Utf8, true)]));
    let column: ArrayRef = Arc::new(StringArray::from(values.to_vec()));
    vec![
        Ok(ResultItem::Schema(Arc::clone(&schema))),
        Ok(ResultItem::Batch(
            RecordBatch::try_new(schema, vec![column]).unwrap(),
        )),
    ]
}

fn apply_ddl(server: &mut Server) {
    if let Some(after) = server.after.take() {
        server.tables.retain(|t| t.obj.name != after.obj.name);
        server.tables.push(after);
    }
}

/// Runs `sql`; `in_tx` marks a pending DDL instead of applying it.
fn answer(
    shared: &Shared,
    database: &str,
    sql: &str,
    cancel: &CancellationToken,
    in_tx: Option<&Mutex<bool>>,
) -> Result<ResultStream> {
    server(shared).log.push(format!("{database}: {sql}"));
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if sql.contains("FAIL") || sql.contains("boom") {
        return Err(Error::sql(Some("42000"), format!("cannot run {sql}")));
    }
    let mut words = sql.split_whitespace();
    let items = match words.next().unwrap_or_default() {
        "CAPTURE" => {
            let value = words.next().filter(|v| *v != "NULL");
            rows(&[value])
        }
        "ROWS" => {
            let n: usize = words.next().and_then(|n| n.parse().ok()).unwrap_or(0);
            let numbers: Vec<String> = (0..n).map(|i| i.to_string()).collect();
            rows(&numbers.iter().map(|s| Some(s.as_str())).collect::<Vec<_>>())
        }
        "CANCEL" => {
            cancel.cancel();
            vec![Ok(ResultItem::RowsAffected(0))]
        }
        "ALTER" | "CREATE" | "RENAME" => {
            match in_tx {
                Some(pending) => *pending.lock().unwrap() = true,
                None => apply_ddl(&mut server(shared)),
            }
            vec![Ok(ResultItem::RowsAffected(0))]
        }
        _ => vec![Ok(ResultItem::RowsAffected(0))],
    };
    Ok(stream::iter(items).boxed())
}

#[async_trait]
impl Connection for ScriptConnection {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    fn server_version(&self) -> &ServerVersion {
        &self.version
    }

    fn current_database(&self) -> Option<String> {
        Some(self.database.clone())
    }

    async fn execute(
        &self,
        sql: &str,
        _params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        answer(&self.server, &self.database, sql, &cancel, None)
    }

    async fn cancel_running(&self) -> Result<()> {
        Ok(())
    }

    async fn begin(&self) -> Result<Box<dyn Tx>> {
        server(&self.server).log.push("BEGIN".into());
        Ok(Box::new(ScriptTx {
            server: Arc::clone(&self.server),
            database: self.database.clone(),
            pending: Mutex::new(false),
        }))
    }

    async fn close(&self) -> Result<()> {
        server(&self.server)
            .log
            .push(format!("close {}", self.database));
        Ok(())
    }
}

struct ScriptTx {
    server: Shared,
    database: String,
    pending: Mutex<bool>,
}

#[async_trait]
impl Tx for ScriptTx {
    async fn execute(
        &self,
        sql: &str,
        _params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        answer(
            &self.server,
            &self.database,
            sql,
            &cancel,
            Some(&self.pending),
        )
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        let mut s = server(&self.server);
        s.log.push("COMMIT".into());
        if *self.pending.lock().unwrap() {
            apply_ddl(&mut s);
        }
        Ok(())
    }

    async fn rollback(self: Box<Self>) -> Result<()> {
        server(&self.server).log.push("ROLLBACK".into());
        Ok(())
    }
}

/// Returns the server's tables (`NotFound` for others).
struct Tables {
    server: Shared,
}

#[async_trait]
impl Fetcher for Tables {
    async fn list(
        &self,
        _c: &dyn Connection,
        parent: &ObjectRef,
        _cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        Ok(server(&self.server)
            .tables
            .iter()
            .map(|t| ObjectSummary::new(parent.child(ObjectKind::Table, t.obj.name.clone())))
            .collect())
    }

    async fn load(
        &self,
        _c: &dyn Connection,
        obj: &ObjectRef,
        _cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        server(&self.server)
            .tables
            .iter()
            .find(|t| t.obj.name == obj.name)
            .cloned()
            .map(ObjectDef::Table)
            .ok_or_else(|| Error::not_found(format!("no table {}", obj.name)))
    }
}

/// `RENAME` per rename, `ALTER TABLE <t> DROP <c>` (destructive), `ALTER TABLE <t> ADD <c>`,
/// `ALTER TABLE <t> ADD FOREIGN KEY <name>`; `CREATE TABLE <t>`. Steps are transactional
/// unless `transactional` is off.
struct ToyDdl {
    transactional: bool,
}

impl ToyDdl {
    fn step(&self, sql: String, description: &str) -> ScriptStep {
        let step = ScriptStep::new(sql).described(description);
        if self.transactional {
            step
        } else {
            step.non_transactional()
        }
    }
}

impl DdlGenerator for ToyDdl {
    fn create(&self, d: &ObjectDef, _ctx: &DdlContext<'_>) -> Result<Script> {
        let ObjectDef::Table(t) = d else {
            return Err(Error::unsupported("only tables"));
        };
        let mut script = Script::new();
        script.push_step(self.step(format!("CREATE TABLE {}", t.obj.name), "Create table"));
        Ok(script)
    }

    fn alter(&self, old: &ObjectDef, new: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        let (ObjectDef::Table(a), ObjectDef::Table(b)) = (old, new) else {
            return Err(Error::unsupported("only tables"));
        };
        assert_eq!(ctx.table, Some(a), "the generator gets the original table");
        let mut script = Script::new();
        for rename in ctx.renames {
            script
                .push_step(self.step(format!("RENAME {} TO {}", rename.old, rename.new), "Rename"));
        }
        let renamed = oxbase_designer::apply_renames(a, ctx.renames);
        for column in &renamed.columns {
            if b.column(&column.name).is_none() {
                script.push_step(
                    self.step(
                        format!("ALTER TABLE {} DROP {}", b.obj.name, column.name),
                        "Drop column",
                    )
                    .destructive(),
                );
            }
        }
        for column in &b.columns {
            if renamed.column(&column.name).is_none() {
                script.push_step(self.step(
                    format!("ALTER TABLE {} ADD {}", b.obj.name, column.name),
                    "Add column",
                ));
            }
        }
        for fk in &b.foreign_keys {
            if !renamed.foreign_keys.contains(fk) {
                script.push_step(self.step(
                    format!(
                        "ALTER TABLE {} ADD FOREIGN KEY {}",
                        b.obj.name,
                        fk.name.as_deref().unwrap_or("-")
                    ),
                    "Add foreign key",
                ));
            }
        }
        Ok(script)
    }

    fn drop_object(&self, r: &ObjectRef, _ctx: &DdlContext<'_>) -> Result<Script> {
        Ok(Script::single(format!("DROP TABLE {}", r.name)))
    }
}

struct ToyCaps {
    transactional: bool,
}

impl Capabilities for ToyCaps {
    fn supports(&self, _kind: ObjectKind, _prop: PropId, _v: &ServerVersion) -> bool {
        true
    }

    fn designer(&self, _v: &ServerVersion) -> DesignerCapabilities {
        let mut notes = Vec::new();
        if !self.transactional {
            notes.push("each DDL statement commits on its own".to_owned());
        }
        DesignerCapabilities {
            transactional_ddl: self.transactional,
            create_table: true,
            drop_table: true,
            rename_table: Support::Native,
            table_comment: Support::Native,
            add_column: Support::Native,
            drop_column: Support::Native,
            rename_column: Support::Native,
            alter_column_type: Support::Native,
            alter_column_nullability: Support::Native,
            alter_column_default: Support::Native,
            column_comment: Support::Native,
            change_primary_key: Support::Native,
            add_index: Support::Native,
            drop_index: Support::Native,
            unique_constraints: true,
            add_foreign_key: Support::Native,
            drop_foreign_key: Support::Native,
            fk_actions: vec![FkAction::Cascade, FkAction::Restrict],
            add_check: Support::Native,
            drop_check: Support::Native,
            checks_enforced: true,
            max_identifier_length: Some(63),
            notes,
            ..DesignerCapabilities::default()
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

fn table(name: &str) -> ObjectRef {
    ObjectRef::table(Some("db1"), Some("public"), name)
}

/// `t (id bigint NOT NULL PRIMARY KEY, name text)` and `parent (id bigint NOT NULL PRIMARY
/// KEY)`.
fn tables() -> Vec<TableDef> {
    let mut t = TableDef::new(table("t"));
    t.columns = vec![
        ColumnDef::new("id", DataType::INT64).not_null(),
        ColumnDef::new("name", DataType::Text),
    ];
    t.primary_key = Some(PrimaryKeyDef {
        name: Some("t_pkey".into()),
        columns: vec!["id".into()],
    });
    let mut parent = TableDef::new(table("parent"));
    parent.columns = vec![ColumnDef::new("id", DataType::INT64).not_null()];
    parent.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec!["id".into()],
    });
    vec![t, parent]
}

struct Harness {
    designer: DesignerService,
    manager: Arc<ConnectionManager>,
    server: Shared,
    session: SessionId,
}

async fn harness(transactional: bool) -> Harness {
    let shared = Shared::default();
    server(&shared).tables = tables();
    let mut drivers = DriverRegistry::new();
    drivers.register(Arc::new(ScriptDriver {
        server: Arc::clone(&shared),
    }));
    let manager = Arc::new(ConnectionManager::new(
        drivers,
        Arc::new(MemorySecretStore::new()),
        Arc::new(RusshTunnelProvider::new()),
    ));
    let mut profile = ConnectionProfile::new(
        "script",
        Engine::Postgres,
        ProfileAddress::Network {
            host: "script.invalid".into(),
            port: Some(5432),
        },
    );
    profile.database = Some("db1".into());
    let base = manager
        .open(&profile, &ProvidedSecrets::none(), CancellationToken::new())
        .await
        .unwrap();
    let mut dialects = DialectRegistry::new();
    dialects
        .register_fetcher(
            Engine::Postgres,
            ObjectKind::Table,
            Arc::new(Tables {
                server: Arc::clone(&shared),
            }),
        )
        .register_ddl(
            Engine::Postgres,
            ObjectKind::Table,
            Arc::new(ToyDdl { transactional }),
        )
        .register_comparator(
            Engine::Postgres,
            ObjectKind::Table,
            Arc::new(RulesComparator(DefaultRules)),
        )
        .register_quoting(Engine::Postgres, Arc::new(PgQuoting))
        .register_capabilities(Engine::Postgres, Arc::new(ToyCaps { transactional }));
    let dialects = Arc::new(dialects);
    let tree = Arc::new(TreeService::new(
        Arc::clone(&manager),
        Arc::clone(&dialects),
    ));
    let designer = DesignerService::new(Arc::clone(&manager), tree, dialects);
    Harness {
        designer,
        manager,
        server: shared,
        session: base.id(),
    }
}

impl Harness {
    fn base(&self) -> SessionHandle {
        self.manager.get(self.session).unwrap()
    }

    fn mark(&self) -> usize {
        server(&self.server).log.len()
    }

    fn log_since(&self, mark: usize) -> Vec<String> {
        server(&self.server).log[mark..].to_vec()
    }

    async fn load(&self) -> TableDesign {
        self.designer
            .load(self.session, &table("t"), CancellationToken::new())
            .await
            .unwrap()
    }

    /// Makes table `t` become `after` once a DDL statement commits.
    fn expect(&self, after: TableDef) {
        server(&self.server).after = Some(after);
    }

    async fn apply(&self, design: &TableDesign, confirm: bool) -> Result<DesignApplyReport> {
        self.designer
            .apply(
                self.session,
                design,
                DesignApplyOptions {
                    confirm_destructive: confirm,
                },
                CancellationToken::new(),
            )
            .await
    }
}

fn add_column(design: &mut TableDesign, name: &str) {
    design.columns.push(DesignColumn {
        origin: None,
        column: ColumnDef::new(name, DataType::Text),
        default_literal: None,
    });
}

fn statuses(outcome: &ScriptOutcome) -> Vec<(usize, StepStatus)> {
    outcome.steps.iter().map(|s| (s.index, s.status)).collect()
}

// ---------------------------------------------------------------------------------------------
// execute_script
// ---------------------------------------------------------------------------------------------

fn rebuild_like(check: &str) -> Script {
    let mut script = Script::new();
    script
        .push_step(
            ScriptStep::new("CAPTURE 1")
                .in_phase(StepPhase::Prepare)
                .capture("fk"),
        )
        .push_step(ScriptStep::new("SET fk off").in_phase(StepPhase::Prepare))
        .push_step(ScriptStep::new("CREATE x").destructive())
        .push_step(
            ScriptStep::new(check)
                .described("Check foreign keys")
                .expect_no_rows()
                .only_if("fk", "1"),
        )
        .push_step(ScriptStep::new("SKIPPED").only_if("fk", "0"))
        .push_step(
            ScriptStep::new("SET fk on")
                .in_phase(StepPhase::Finally)
                .only_if("fk", "1"),
        );
    script
}

#[tokio::test]
async fn scripts_run_prepare_main_in_one_transaction_and_finally() {
    let h = harness(true).await;
    let mark = h.mark();
    let outcome = execute_script(
        &h.base(),
        &rebuild_like("ROWS 0"),
        true,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(outcome.committed && outcome.atomic, "{outcome:?}");
    assert_eq!(
        h.log_since(mark),
        [
            "db1: CAPTURE 1",
            "db1: SET fk off",
            "BEGIN",
            "db1: CREATE x",
            "db1: ROWS 0",
            "COMMIT",
            "db1: SET fk on",
        ]
    );
    assert_eq!(
        statuses(&outcome),
        [
            (0, StepStatus::Done),
            (1, StepStatus::Done),
            (2, StepStatus::Done),
            (3, StepStatus::Done),
            (4, StepStatus::Skipped),
            (5, StepStatus::Done),
        ]
    );
    // Execution order is the script's phase order, whatever the order of `steps`.
    let mut shuffled = rebuild_like("ROWS 0");
    shuffled.steps.rotate_left(5);
    let mark = h.mark();
    let outcome = execute_script(&h.base(), &shuffled, true, CancellationToken::new())
        .await
        .unwrap();
    assert!(outcome.committed);
    assert_eq!(
        h.log_since(mark).last().map(String::as_str),
        Some("db1: SET fk on")
    );
    assert_eq!(outcome.steps[0].index, 1);

    // A capture of NULL (or no row) is the empty string: the conditional steps are skipped.
    let mut script = rebuild_like("ROWS 0");
    script.steps[0].sql = "CAPTURE NULL".into();
    let outcome = execute_script(&h.base(), &script, true, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        statuses(&outcome)
            .iter()
            .filter(|(_, s)| *s == StepStatus::Skipped)
            .count(),
        3
    );
}

#[tokio::test]
async fn a_failed_check_rolls_back_and_finally_still_runs() {
    let h = harness(true).await;
    let mark = h.mark();
    let mut script = rebuild_like("ROWS 2");
    script.push_step(ScriptStep::new("AFTER"));
    let outcome = execute_script(&h.base(), &script, true, CancellationToken::new())
        .await
        .unwrap();
    assert!(!outcome.committed);
    assert_eq!(
        statuses(&outcome),
        [
            (0, StepStatus::Done),
            (1, StepStatus::Done),
            (2, StepStatus::RolledBack),
            (3, StepStatus::Failed),
            (4, StepStatus::NotRun),
            (6, StepStatus::NotRun),
            (5, StepStatus::Done),
        ]
    );
    let error = outcome.steps[3].error.as_ref().unwrap();
    assert!(
        error
            .message
            .contains("\"Check foreign keys\" found 2 problem row(s): n=0; n=1"),
        "{error:?}"
    );
    let log = h.log_since(mark);
    assert!(log.contains(&"ROLLBACK".to_owned()) && !log.contains(&"COMMIT".to_owned()));
    assert_eq!(log.last().map(String::as_str), Some("db1: SET fk on"));
}

#[tokio::test]
async fn non_atomic_scripts_stop_at_the_first_failure() {
    let h = harness(false).await;
    let mut script = Script::new();
    script
        .push_step(ScriptStep::new("A1").non_transactional())
        .push_step(ScriptStep::new("FAIL here").non_transactional())
        .push_step(ScriptStep::new("A3").non_transactional())
        .push_step(ScriptStep::new("FAIL cleanup").in_phase(StepPhase::Finally));
    let mark = h.mark();
    let outcome = execute_script(&h.base(), &script, false, CancellationToken::new())
        .await
        .unwrap();
    assert!(!outcome.committed && !outcome.atomic);
    assert_eq!(
        statuses(&outcome),
        [
            (0, StepStatus::Done),
            (1, StepStatus::Failed),
            (2, StepStatus::NotRun),
            (3, StepStatus::Failed),
        ]
    );
    assert_eq!(
        h.log_since(mark),
        ["db1: A1", "db1: FAIL here", "db1: FAIL cleanup"]
    );
    assert_eq!(outcome.warnings.len(), 1);
    assert!(outcome.warnings[0].contains("a clean-up step failed"));

    // A failing prepare step runs no main step.
    let mut script = Script::new();
    script
        .push_step(ScriptStep::new("FAIL early").in_phase(StepPhase::Prepare))
        .push_step(ScriptStep::new("MAIN"))
        .push_step(ScriptStep::new("CLEANUP").in_phase(StepPhase::Finally));
    let outcome = execute_script(&h.base(), &script, true, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        statuses(&outcome),
        [
            (0, StepStatus::Failed),
            (1, StepStatus::NotRun),
            (2, StepStatus::Done),
        ]
    );
    assert!(!outcome.committed);
}

#[tokio::test]
async fn cancelling_stops_before_the_next_step_and_rolls_back() {
    let h = harness(true).await;
    let mut script = Script::new();
    script
        .push_step(ScriptStep::new("CANCEL now"))
        .push_step(ScriptStep::new("NEVER"))
        .push_step(ScriptStep::new("CLEANUP").in_phase(StepPhase::Finally));
    let mark = h.mark();
    let outcome = execute_script(&h.base(), &script, true, CancellationToken::new())
        .await
        .unwrap();
    assert!(!outcome.committed);
    assert_eq!(
        statuses(&outcome),
        [
            (0, StepStatus::RolledBack),
            (1, StepStatus::Failed),
            (2, StepStatus::Done),
        ]
    );
    assert_eq!(
        outcome.steps[1].error.as_ref().unwrap().category,
        ErrorCategory::Cancelled
    );
    assert_eq!(
        h.log_since(mark),
        ["BEGIN", "db1: CANCEL now", "ROLLBACK", "db1: CLEANUP"]
    );
    // A token cancelled before the start runs nothing.
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let err = execute_script(&h.base(), &script, true, cancelled)
        .await
        .unwrap_err();
    assert!(err.is_cancelled());
}

// ---------------------------------------------------------------------------------------------
// DesignerService
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn load_and_preview() {
    let h = harness(true).await;
    let design = h.load().await;
    assert_eq!(design.columns.len(), 2);
    assert_eq!(design.original.as_ref().unwrap().obj, table("t"));
    let err = h
        .designer
        .load(
            h.session,
            &ObjectRef::new(ObjectKind::View, "v"),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);

    let mut edited = design.clone();
    add_column(&mut edited, "note");
    edited.columns[1].column.name = "title".into();
    let preview = h
        .designer
        .preview(h.session, &edited, CancellationToken::new())
        .await
        .unwrap();
    assert!(!preview.has_errors(), "{:?}", preview.issues);
    // The rename is a `name` difference, not a drop and an add.
    assert_eq!(
        preview.diffs[0].prop.as_ref().map(PropId::as_str),
        Some("name")
    );
    assert_eq!(
        preview
            .script
            .steps
            .iter()
            .map(|s| s.sql.as_str())
            .collect::<Vec<_>>(),
        ["RENAME name TO title", "ALTER TABLE t ADD note"]
    );
    assert!(preview.atomic && !preview.destructive);
    assert_eq!(
        preview.sql,
        "BEGIN;\n-- Rename\nRENAME name TO title;\n-- Add column\nALTER TABLE t ADD note;\nCOMMIT;\n"
    );

    // Without transactional DDL the preview says so.
    let h = harness(false).await;
    let mut edited = h.load().await;
    add_column(&mut edited, "note");
    let preview = h
        .designer
        .preview(h.session, &edited, CancellationToken::new())
        .await
        .unwrap();
    assert!(!preview.atomic);
    assert!(
        preview.notes.contains(&NOT_ATOMIC_NOTE.to_owned()),
        "{:?}",
        preview.notes
    );
    assert_eq!(preview.sql, "-- Add column\nALTER TABLE t ADD note;\n");
}

#[tokio::test]
async fn foreign_key_targets_are_checked_on_the_server() {
    let h = harness(true).await;
    let design = h.load().await;
    let fk = |name: &str, column: &str, target: &str| {
        DesignItem::new(ForeignKeyDef {
            name: Some(name.into()),
            columns: vec![column.into()],
            ref_table: ObjectRef::table(None, Some("public"), target),
            ref_columns: vec!["id".into()],
            on_delete: FkAction::Cascade,
            on_update: FkAction::NoAction,
            extras: Extras::new(),
        })
    };
    let preview_of = |fks: Vec<DesignItem<ForeignKeyDef>>| {
        let mut edited = design.clone();
        edited.foreign_keys = fks;
        let designer = &h.designer;
        let session = h.session;
        async move {
            designer
                .preview(session, &edited, CancellationToken::new())
                .await
                .unwrap()
        }
    };
    let preview = preview_of(vec![fk("t_parent_fk", "id", "parent")]).await;
    assert_eq!(preview.issues, []);
    assert_eq!(preview.script.len(), 1);

    let preview = preview_of(vec![fk("t_missing_fk", "id", "missing")]).await;
    assert_eq!(preview.issues.len(), 1, "{:?}", preview.issues);
    assert_eq!(preview.issues[0].path, "foreignKeys[0].refTable");
    assert!(preview.issues[0].message.contains("does not exist"));
    assert!(preview.script.is_empty() && preview.sql.is_empty());

    // `name` is text, `parent.id` an integer.
    let preview = preview_of(vec![fk("t_name_fk", "name", "parent")]).await;
    assert_eq!(preview.issues.len(), 1, "{:?}", preview.issues);
    assert_eq!(preview.issues[0].path, "foreignKeys[0].refColumns[0]");
    assert!(preview.has_errors());
    let err = h
        .designer
        .apply(
            h.session,
            &{
                let mut edited = design.clone();
                edited.foreign_keys = vec![fk("t_name_fk", "name", "parent")];
                edited
            },
            DesignApplyOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
}

#[tokio::test]
async fn apply_refuses_errors_unconfirmed_drops_no_changes_and_a_changed_table() {
    let h = harness(true).await;
    let design = h.load().await;
    let mark = h.mark();

    let mut invalid = design.clone();
    add_column(&mut invalid, "id");
    let err = h.apply(&invalid, true).await.unwrap_err();
    assert!(
        err.to_string().contains("already a column named 'id'"),
        "{err}"
    );

    let err = h.apply(&design, true).await.unwrap_err();
    assert!(err.to_string().contains("nothing to apply"), "{err}");

    let mut dropped = design.clone();
    dropped.columns.pop();
    let err = h.apply(&dropped, false).await.unwrap_err();
    assert!(err.to_string().contains("confirm"), "{err}");

    // Someone added a column since the design was loaded.
    let mut changed = tables();
    changed[0]
        .columns
        .push(ColumnDef::new("other", DataType::Text));
    server(&h.server).tables = changed;
    let mut edited = design.clone();
    add_column(&mut edited, "note");
    let err = h.apply(&edited, false).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert!(err.to_string().contains(CHANGED_SINCE_LOADED), "{err}");
    // Gone altogether.
    server(&h.server).tables.clear();
    let err = h.apply(&edited, false).await.unwrap_err();
    assert!(err.to_string().contains("no longer exists"), "{err}");
    // A new table must not exist yet.
    server(&h.server).tables = tables();
    let mut new = TableDesign::new_table(table("parent"));
    add_column(&mut new, "id");
    let err = h.apply(&new, false).await.unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");

    // Nothing ran.
    assert!(
        h.log_since(mark)
            .iter()
            .all(|l| !l.contains("ALTER") && !l.contains("CREATE")),
        "{:?}",
        h.log_since(mark)
    );
}

#[tokio::test]
async fn apply_runs_on_a_dedicated_session_and_reloads() {
    let h = harness(true).await;
    let design = h.load().await;
    let mut edited = design.clone();
    add_column(&mut edited, "note");
    let expected = edited.to_def(&PgQuoting);
    h.expect(expected.clone());
    let mark = h.mark();
    let report = h.apply(&edited, false).await.unwrap();
    assert!(
        report.outcome.committed && report.outcome.atomic,
        "{report:?}"
    );
    assert_eq!(statuses(&report.outcome), [(0, StepStatus::Done)]);
    assert_eq!(
        h.log_since(mark),
        [
            "connect db1",
            "BEGIN",
            "db1: ALTER TABLE t ADD note",
            "COMMIT",
            "close db1",
        ]
    );
    // The reloaded design is the table as it is now, equal to the edited one.
    let reloaded = report.design.unwrap();
    assert_eq!(reloaded, TableDesign::from_def(expected.clone()));
    assert_eq!(reloaded.to_def(&PgQuoting), expected);

    // A new table.
    let mut new = TableDesign::new_table(table("fresh"));
    add_column(&mut new, "a");
    let created = new.to_def(&PgQuoting);
    h.expect(created.clone());
    let report = h.apply(&new, false).await.unwrap();
    assert!(report.outcome.committed);
    assert_eq!(report.design, Some(TableDesign::from_def(created)));
}

#[tokio::test]
async fn failed_applies_report_steps_and_reload_only_when_not_atomic() {
    // Atomic: everything rolls back, nothing to reload.
    let h = harness(true).await;
    let mut edited = h.load().await;
    add_column(&mut edited, "note");
    add_column(&mut edited, "boom");
    let report = h.apply(&edited, false).await.unwrap();
    assert!(!report.outcome.committed);
    assert_eq!(
        statuses(&report.outcome),
        [(0, StepStatus::RolledBack), (1, StepStatus::Failed)]
    );
    assert_eq!(report.design, None);
    assert_eq!(server(&h.server).tables, tables());

    // Not atomic: the first step stays applied and the design is reloaded.
    let h = harness(false).await;
    let mut edited = h.load().await;
    add_column(&mut edited, "note");
    add_column(&mut edited, "boom");
    let mut half = tables()[0].clone();
    half.columns.push(ColumnDef::new("note", DataType::Text));
    h.expect(half.clone());
    let report = h.apply(&edited, false).await.unwrap();
    assert!(!report.outcome.committed && !report.outcome.atomic);
    assert_eq!(
        statuses(&report.outcome),
        [(0, StepStatus::Done), (1, StepStatus::Failed)]
    );
    assert_eq!(report.design, Some(TableDesign::from_def(half)));
}

// ---------------------------------------------------------------------------------------------
// The default registry
// ---------------------------------------------------------------------------------------------

/// Every M3 engine registers all designer and grid-editing roles, or (until its stream lands)
/// none of the M3 ones; with `OXBASE_SERVICES_REQUIRE_M3_CELLS=1` none missing is required.
#[test]
fn default_dialects_have_every_designer_role() {
    let registry = crate::default_dialects();
    let require = std::env::var("OXBASE_SERVICES_REQUIRE_M3_CELLS").is_ok_and(|v| v == "1");
    // Nothing of M3: a generator and a comparator per designer kind, and the DML writer.
    let nothing = 2 * oxbase_dialect::DESIGNER_KINDS.len() + 1;
    for engine in [
        Engine::Postgres,
        Engine::MySql,
        Engine::MariaDb,
        Engine::Sqlite,
    ] {
        let missing = registry.missing_designer_roles(engine);
        let pending = !require && missing.len() == nothing;
        assert!(
            missing.is_empty() || pending,
            "{engine}: partly registered: {missing:?}"
        );
    }
}
