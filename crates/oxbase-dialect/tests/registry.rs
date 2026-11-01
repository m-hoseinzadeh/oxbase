//! DialectRegistry behaviour, exercised with toy cells over the mock driver.

use std::sync::Arc;

use arrow_array::cast::AsArray;
use async_trait::async_trait;
use oxbase_dialect::sql::SqlWriter;
use oxbase_dialect::{
    AnsiQuoting, Capabilities, CellRole, CellStatus, ChildGroup, Comparator, DESIGNER_KINDS,
    DatabaseScope, DdlContext, DdlGenerator, DialectRegistry, DmlPlan, DmlTarget, DmlWriter,
    EngineRole, Fetcher, MissingCell, PgQuoting, Quoting, Support, TreeLayout, write_key_predicate,
};
use oxbase_driver::mock::MockDriver;
use oxbase_driver::{CancellationToken, ConnectParams, Connection, Driver, collect};
use oxbase_types::{
    ColumnDef, DataType, Engine, Error, ErrorCategory, ObjectDef, ObjectKind, ObjectRef,
    ObjectSummary, PropId, PropertyDiff, Result, RowEdit, RowKey, Script, ScriptStep,
    ServerVersion, TableDef, Value, props,
};
use pretty_assertions::assert_eq;

/// Lists "tables" from the mock's canned `(id, name)` rows.
struct ToyTableFetcher;

#[async_trait]
impl Fetcher for ToyTableFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let stream = c.execute("SELECT name FROM tables", &[], cancel).await?;
        let mut out = Vec::new();
        for set in collect(stream).await?.result_sets {
            for batch in set.batches {
                let names = batch.column(1).as_string::<i32>();
                for name in names.iter().flatten() {
                    out.push(ObjectSummary::new(parent.child(ObjectKind::Table, name)));
                }
            }
        }
        Ok(out)
    }

    async fn load(
        &self,
        _c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut t = TableDef::new(obj.clone());
        t.columns
            .push(ColumnDef::new("id", DataType::INT64).not_null());
        Ok(ObjectDef::Table(t))
    }
}

struct ToyComparator;

impl Comparator for ToyComparator {
    fn diff(&self, a: &ObjectDef, b: &ObjectDef) -> Vec<PropertyDiff> {
        let (ObjectDef::Table(a), ObjectDef::Table(b)) = (a, b) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for col in &b.columns {
            if a.column(&col.name).is_none() {
                out.push(PropertyDiff::object_added(
                    b.obj.child(ObjectKind::Column, &col.name),
                ));
            }
        }
        out
    }
}

struct ToyDdl;

impl DdlGenerator for ToyDdl {
    fn create(&self, d: &ObjectDef, _ctx: &DdlContext<'_>) -> Result<Script> {
        Ok(Script::single(format!(
            "CREATE TABLE {}",
            AnsiQuoting.qualify(&d.object_ref())
        )))
    }

    fn alter(&self, old: &ObjectDef, new: &ObjectDef, _ctx: &DdlContext<'_>) -> Result<Script> {
        let mut s = Script::new();
        for d in ToyComparator.diff(old, new) {
            s.push_step(
                ScriptStep::new(format!(
                    "ALTER TABLE {} ADD COLUMN {}",
                    AnsiQuoting.qualify(&old.object_ref()),
                    AnsiQuoting.quote_ident(&d.object.name)
                ))
                .described(format!("Add column {}", d.object.name)),
            );
        }
        Ok(s)
    }

    fn drop_object(&self, r: &ObjectRef, _ctx: &DdlContext<'_>) -> Result<Script> {
        let mut s = Script::new();
        s.push_destructive(format!("DROP TABLE {}", AnsiQuoting.qualify(r)));
        Ok(s)
    }
}

struct ToyCaps;

impl Capabilities for ToyCaps {
    fn supports(&self, kind: ObjectKind, prop: PropId, v: &ServerVersion) -> bool {
        kind == ObjectKind::Table && prop == props::UNLOGGED && v.at_least(9, 1)
    }

    fn supports_kind(&self, kind: ObjectKind, _v: &ServerVersion) -> bool {
        kind != ObjectKind::Event
    }
}

struct ToyDml;

impl DmlWriter for ToyDml {
    fn plan(&self, edit: &RowEdit, target: &DmlTarget<'_>) -> Result<DmlPlan> {
        let q = PgQuoting;
        let mut w = SqlWriter::new(&q);
        match edit {
            RowEdit::Delete { key } => {
                w.push("DELETE FROM ")
                    .qualified(&target.key.table)
                    .push(" WHERE ");
                write_key_predicate(&mut w, target.key, key)?;
                Ok(DmlPlan::write_only(w.finish()))
            }
            _ => Err(Error::unsupported("toy writer only deletes")),
        }
    }
}

fn full_postgres_table() -> DialectRegistry {
    let mut reg = DialectRegistry::new();
    reg.register_fetcher(
        Engine::Postgres,
        ObjectKind::Table,
        Arc::new(ToyTableFetcher),
    )
    .register_ddl(Engine::Postgres, ObjectKind::Table, Arc::new(ToyDdl))
    .register_comparator(Engine::Postgres, ObjectKind::Table, Arc::new(ToyComparator))
    .register_capabilities(Engine::Postgres, Arc::new(ToyCaps))
    .register_quoting(Engine::Postgres, Arc::new(PgQuoting))
    .register_dml_writer(Engine::Postgres, Arc::new(ToyDml));
    reg
}

#[test]
fn empty_registry_reports_every_cell_missing() {
    let reg = DialectRegistry::new();
    let missing = reg.missing_cells(&[Engine::Postgres, Engine::Redis]);
    assert_eq!(
        missing.len(),
        2 * ObjectKind::ALL.len() * CellRole::ALL.len()
    );
    assert_eq!(
        missing[0],
        MissingCell {
            engine: Engine::Postgres,
            kind: ObjectKind::Database,
            role: CellRole::Fetcher
        }
    );
    assert_eq!(missing[0].to_string(), "postgres/database: no Fetcher");
    assert_eq!(reg.missing_engine_roles(&[Engine::MySql]).len(), 3);

    let err = reg
        .fetcher(Engine::Postgres, ObjectKind::Table)
        .err()
        .unwrap();
    assert_eq!(err.category(), ErrorCategory::Unsupported);
    assert_eq!(
        err.to_string(),
        "unsupported: PostgreSQL has no Fetcher for Table"
    );
    assert!(reg.quoting(Engine::Sqlite).is_err());
    assert!(reg.capabilities(Engine::Sqlite).is_err());
    assert!(reg.dml_writer(Engine::Sqlite).is_err());
}

#[test]
fn registered_cells_and_not_applicable_are_not_missing() {
    let mut reg = full_postgres_table();
    reg.register_fetcher(Engine::Sqlite, ObjectKind::Table, Arc::new(ToyTableFetcher))
        .mark_not_applicable(Engine::Sqlite, ObjectKind::Sequence)
        .mark_not_applicable(Engine::Sqlite, ObjectKind::Table); // overridden by the fetcher

    assert_eq!(
        reg.status(Engine::Postgres, ObjectKind::Table),
        CellStatus::Roles(CellRole::ALL.to_vec())
    );
    assert!(
        reg.status(Engine::Postgres, ObjectKind::Table)
            .is_complete()
    );
    assert_eq!(
        reg.status(Engine::Sqlite, ObjectKind::Sequence),
        CellStatus::NotApplicable
    );
    assert_eq!(
        reg.status(Engine::Sqlite, ObjectKind::Table),
        CellStatus::Roles(vec![CellRole::Fetcher])
    );

    let pg_missing = reg.missing_cells(&[Engine::Postgres]);
    assert_eq!(pg_missing.len(), (ObjectKind::ALL.len() - 1) * 3);
    assert!(pg_missing.iter().all(|m| m.kind != ObjectKind::Table));

    let lite_missing = reg.missing_cells(&[Engine::Sqlite]);
    // Every kind but Table and Sequence is empty (3 roles each), + Table missing DdlGenerator
    // and Comparator; Sequence is n/a.
    assert_eq!(lite_missing.len(), (ObjectKind::ALL.len() - 2) * 3 + 2);
    assert!(lite_missing.iter().all(|m| m.kind != ObjectKind::Sequence));

    assert_eq!(reg.engine_roles(Engine::Postgres), EngineRole::ALL.to_vec());
    assert_eq!(reg.missing_engine_roles(&[Engine::Postgres]), []);
    assert!(reg.ddl(Engine::Sqlite, ObjectKind::Table).is_err());
    assert!(format!("{reg:?}").contains("postgres/table:FDC"));
}

#[test]
fn matrix_renders_the_kind_by_engine_grid() {
    let mut reg = full_postgres_table();
    reg.register_fetcher(Engine::MySql, ObjectKind::View, Arc::new(ToyTableFetcher))
        .mark_not_applicable(Engine::MySql, ObjectKind::Sequence);
    let m = reg.matrix(&[Engine::Postgres, Engine::MySql]);
    assert_eq!(m.kinds.len(), ObjectKind::ALL.len());
    assert_eq!(
        m.get(Engine::Postgres, ObjectKind::Table)
            .map(CellStatus::code),
        Some("FDC".to_owned())
    );
    assert_eq!(
        m.get(Engine::MySql, ObjectKind::View).map(CellStatus::code),
        Some("F--".to_owned())
    );
    assert_eq!(
        m.get(Engine::MySql, ObjectKind::Sequence)
            .map(CellStatus::code),
        Some("n/a".to_owned())
    );
    assert_eq!(m.get(Engine::Redis, ObjectKind::Table), None);
    // 2 x ObjectKind::ALL cells, minus pg/table (complete) and mysql/sequence (n/a).
    assert_eq!(m.incomplete(), 2 * ObjectKind::ALL.len() - 2);

    let text = m.to_string();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines.len(),
        1 + ObjectKind::ALL.len() + EngineRole::ALL.len()
    );
    assert_eq!(lines[0].trim(), "postgres  mysql");
    assert!(text.contains("\ntable         FDC       ---\n"), "{text}");
    assert!(text.contains("\nsequence      ---       n/a\n"), "{text}");
    assert!(text.contains("\nQuoting       yes       -\n"), "{text}");
}

#[tokio::test]
async fn cells_work_through_the_registry() {
    let reg = full_postgres_table();
    let driver = MockDriver::new(Engine::Postgres).with_version(ServerVersion::new(16, 2, 0));
    let conn = driver
        .connect(&ConnectParams::network(Engine::Postgres, "mock"))
        .await
        .unwrap();

    // Fetcher: list + load.
    let fetcher = reg.fetcher(conn.engine(), ObjectKind::Table).unwrap();
    let schema = ObjectRef::schema(Some("shop"), "public");
    let tables = fetcher
        .list(conn.as_ref(), &schema, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(tables.len(), 12);
    assert_eq!(
        tables[0].obj,
        ObjectRef::table(Some("shop"), Some("public"), "row 0")
    );
    let def = fetcher
        .load(conn.as_ref(), &tables[0].obj, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(def.kind(), ObjectKind::Table);

    // Both methods honour an already-cancelled token and return no partial result.
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let err = fetcher
        .list(conn.as_ref(), &schema, cancelled.clone())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Cancelled);
    let err = fetcher
        .load(conn.as_ref(), &tables[0].obj, cancelled)
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Cancelled);

    // Comparator + DdlGenerator.
    let mut newer = def.clone();
    if let ObjectDef::Table(t) = &mut newer {
        t.columns.push(ColumnDef::new("note", DataType::Text));
    }
    let diffs = reg
        .comparator(Engine::Postgres, ObjectKind::Table)
        .unwrap()
        .diff(&def, &newer);
    assert_eq!(diffs.len(), 1);
    let ddl = reg.ddl(Engine::Postgres, ObjectKind::Table).unwrap();
    let ctx = DdlContext::new(conn.server_version());
    assert_eq!(
        ddl.create(&def, &ctx).unwrap().to_string(),
        "CREATE TABLE \"shop\".\"public\".\"row 0\";\n"
    );
    let alter = ddl.alter(&def, &newer, &ctx).unwrap();
    assert_eq!(
        alter.to_string(),
        "ALTER TABLE \"shop\".\"public\".\"row 0\" ADD COLUMN \"note\";\n"
    );
    assert_eq!(alter.steps[0].description, "Add column note");
    assert!(alter.is_atomic());
    // Callable straight on the `Arc<dyn DdlGenerator>` (no clash with `Drop::drop`).
    assert!(
        ddl.drop_object(&tables[0].obj, &ctx)
            .unwrap()
            .is_destructive()
    );

    // Capabilities gate extras by version.
    let caps = reg.capabilities(Engine::Postgres).unwrap();
    assert!(caps.supports(ObjectKind::Table, props::UNLOGGED, conn.server_version()));
    assert!(!caps.supports(
        ObjectKind::Table,
        props::UNLOGGED,
        &ServerVersion::new(9, 0, 0)
    ));
    assert!(!caps.supports_kind(ObjectKind::Event, conn.server_version()));
    // Engines that do not describe their designer get a read-only one.
    assert_eq!(
        caps.designer(conn.server_version()).add_column,
        Support::Unsupported
    );

    // DmlWriter: values are bound, never inlined.
    let key = RowKey::new(
        ObjectRef::table(None, Some("public"), "t"),
        oxbase_types::KeyKind::Primary,
        vec!["id".into()],
    );
    let edit = RowEdit::Delete {
        key: vec![Value::Int(7)],
    };
    key.validate(&edit).unwrap();
    let table = TableDef::new(key.table.clone());
    let target = DmlTarget {
        table: &table,
        key: &key,
        version: conn.server_version(),
        read_back: &[],
    };
    let plan = reg
        .dml_writer(Engine::Postgres)
        .unwrap()
        .plan(&edit, &target)
        .unwrap();
    assert_eq!(
        plan.write.sql,
        "DELETE FROM \"public\".\"t\" WHERE \"id\" = $1"
    );
    assert_eq!(plan.write.params, [Value::Int(7)]);
    assert!(!plan.returning && plan.read_back.is_none());
    assert_eq!(
        reg.quoting(Engine::Postgres).unwrap().quote_ident("x"),
        "\"x\""
    );
    assert_eq!(driver.log(), ["SELECT name FROM tables"]);
}

#[test]
fn tree_layouts_and_applicability_helpers() {
    use ObjectKind as K;
    let mut reg = full_postgres_table();
    assert!(reg.tree_layout(Engine::Postgres).is_err());
    assert_eq!(reg.missing_layout_fetchers(Engine::Postgres), []);

    let layout = TreeLayout::new(DatabaseScope::Session)
        .with_root(&[ChildGroup::direct(K::Database)])
        .with_children(K::Database, &[ChildGroup::direct(K::Schema)])
        .with_children(K::Schema, &[ChildGroup::folder(K::Table)]);
    reg.register_tree_layout(Engine::Postgres, layout.clone());
    assert_eq!(*reg.tree_layout(Engine::Postgres).unwrap(), layout);
    // Only Table has a fetcher so far.
    assert_eq!(
        reg.missing_layout_fetchers(Engine::Postgres),
        [K::Database, K::Schema]
    );
    // The layout is data, not a matrix role: the engine roles are unchanged.
    assert_eq!(reg.engine_roles(Engine::Postgres), EngineRole::ALL.to_vec());

    // ToyCaps says Event does not exist; engines without capabilities support everything.
    let v = ServerVersion::new(16, 0, 0);
    assert!(!reg.supports_kind(Engine::Postgres, K::Event, &v));
    assert!(reg.supports_kind(Engine::Postgres, K::Table, &v));
    assert!(reg.supports_kind(Engine::Sqlite, K::Event, &v));

    reg.mark_not_applicable_except(Engine::Sqlite, &[K::Database, K::Table, K::View]);
    assert_eq!(
        reg.status(Engine::Sqlite, K::Sequence),
        CellStatus::NotApplicable
    );
    assert_eq!(
        reg.status(Engine::Sqlite, K::View),
        CellStatus::Roles(vec![])
    );
    assert_eq!(reg.engines(), [Engine::Postgres, Engine::Sqlite]);
}

#[test]
fn missing_designer_roles_lists_what_m3_needs() {
    let reg = full_postgres_table();
    let missing = reg.missing_designer_roles(Engine::Postgres);
    // Table has every role; the four child kinds lack DdlGenerator and Comparator.
    assert_eq!(missing.len(), (DESIGNER_KINDS.len() - 1) * 2);
    assert!(missing.contains(&"postgres/column: no DdlGenerator".to_owned()));
    assert!(!missing.iter().any(|m| m.starts_with("postgres/table")));
    let empty = DialectRegistry::new().missing_designer_roles(Engine::Sqlite);
    assert_eq!(
        empty.len(),
        DESIGNER_KINDS.len() * 2 + EngineRole::ALL.len()
    );
    assert!(empty.contains(&"sqlite: no DmlWriter".to_owned()));
}

#[test]
fn bulk_loader_falls_back_to_multi_row_inserts() {
    use oxbase_dialect::{BulkLimits, BulkMethod};

    let mut reg = DialectRegistry::new();
    let v = ServerVersion::new(16, 0, 0);
    let err = reg.bulk_loader(Engine::Postgres).err().unwrap();
    assert_eq!(err.category(), ErrorCategory::Unsupported);

    reg.register_quoting(Engine::Postgres, Arc::new(PgQuoting));
    assert!(!reg.has_bulk_loader(Engine::Postgres));
    let fallback = reg.bulk_loader(Engine::Postgres).unwrap();
    assert_eq!(fallback.method(&v), BulkMethod::MultiRowInsert);
    assert_eq!(
        fallback.limits(&v),
        BulkLimits::for_engine(Engine::Postgres)
    );
    // The bulk loader is not a matrix role: the engine roles are unchanged.
    assert_eq!(
        reg.engine_roles(Engine::Postgres),
        vec![EngineRole::Quoting]
    );

    let mut limits = BulkLimits::for_engine(Engine::Postgres);
    limits.rows_per_call = 7;
    reg.register_bulk_loader(
        Engine::Postgres,
        Arc::new(oxbase_dialect::InsertLoader::new(
            Arc::new(PgQuoting),
            limits,
        )),
    );
    assert!(reg.has_bulk_loader(Engine::Postgres));
    assert_eq!(
        reg.bulk_loader(Engine::Postgres)
            .unwrap()
            .limits(&v)
            .rows_per_call,
        7
    );
}

#[tokio::test]
async fn insert_loader_runs_one_statement_per_chunk_in_the_transaction() {
    use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
    use oxbase_dialect::{
        BulkLimits, BulkLoader, InsertLoader, LoadColumn, LoadSession, LoadTarget,
    };

    let driver = MockDriver::new(Engine::Sqlite);
    let conn = driver
        .connect(&ConnectParams::network(Engine::Sqlite, "mock"))
        .await
        .unwrap();
    let tx = conn.begin().await.unwrap();

    let table = ObjectRef::table(Some("main"), None, "t");
    let columns = vec![
        LoadColumn::new("id", DataType::INT64),
        LoadColumn::new("name", DataType::Text),
    ];
    let v = ServerVersion::new(3, 54, 1);
    let target = LoadTarget {
        table: &table,
        columns: &columns,
        version: &v,
    };
    let ids: ArrayRef = Arc::new(Int64Array::from_iter_values(0..5));
    let names: ArrayRef = Arc::new(StringArray::from(vec!["a", "b", "c", "d", "e"]));
    let batch = RecordBatch::try_new(target.schema(), vec![ids, names]).unwrap();

    let mut limits = BulkLimits::for_engine(Engine::Sqlite);
    limits.max_rows_per_statement = 2;
    let loader = InsertLoader::new(Arc::new(AnsiQuoting), limits);
    let session = LoadSession::new(conn.as_ref(), Some(tx.as_ref()));
    let sent = loader
        .load(session, &target, &batch, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(sent, 5);
    tx.commit().await.unwrap();

    let log = driver.log();
    let inserts: Vec<&String> = log.iter().filter(|s| s.starts_with("INSERT")).collect();
    assert_eq!(inserts.len(), 3);
    assert_eq!(
        inserts[0].as_str(),
        "INSERT INTO \"main\".\"t\" (\"id\", \"name\") VALUES (?, ?), (?, ?)"
    );
    assert_eq!(log.first().map(String::as_str), Some("BEGIN"));
    assert_eq!(log.last().map(String::as_str), Some("COMMIT"));

    // A cancelled token stops before the first statement.
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let err = loader
        .load(
            LoadSession::new(conn.as_ref(), None),
            &target,
            &batch,
            cancelled,
        )
        .await
        .unwrap_err();
    assert!(err.is_cancelled());
}
