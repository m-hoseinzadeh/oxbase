//! DialectRegistry behaviour, exercised with toy cells over the mock driver.

use std::sync::Arc;

use arrow_array::cast::AsArray;
use async_trait::async_trait;
use oxbase_dialect::{
    AnsiQuoting, Capabilities, CellRole, CellStatus, Comparator, DdlGenerator, DialectRegistry,
    DmlWriter, EngineRole, Fetcher, MissingCell, PgQuoting, Quoting,
};
use oxbase_driver::mock::MockDriver;
use oxbase_driver::{CancellationToken, ConnectParams, Connection, Driver, collect};
use oxbase_types::{
    ColumnDef, DataType, Engine, Error, ErrorCategory, ObjectDef, ObjectKind, ObjectRef,
    ObjectSummary, PropId, PropertyDiff, Result, RowEdit, RowKey, Script, ServerVersion, TableDef,
    Value, props,
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
    fn create(&self, d: &ObjectDef) -> Script {
        Script::single(format!(
            "CREATE TABLE {}",
            AnsiQuoting.qualify(&d.object_ref())
        ))
    }

    fn alter(&self, old: &ObjectDef, new: &ObjectDef) -> Script {
        let mut s = Script::new();
        for d in ToyComparator.diff(old, new) {
            s.push(format!(
                "ALTER TABLE {} ADD COLUMN {}",
                AnsiQuoting.qualify(&old.object_ref()),
                AnsiQuoting.quote_ident(&d.object.name)
            ));
        }
        s
    }

    fn drop_object(&self, r: &ObjectRef) -> Script {
        let mut s = Script::new();
        s.push_destructive(format!("DROP TABLE {}", AnsiQuoting.qualify(r)));
        s
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
    fn apply(&self, edits: &[RowEdit], key: &RowKey) -> Script {
        let q = AnsiQuoting;
        edits
            .iter()
            .filter_map(|e| match e {
                RowEdit::Delete { key: vals } => {
                    let cond: Vec<String> = key
                        .bind(vals)
                        .map(|(c, v)| format!("{} = {}", q.quote_ident(c), q.literal(v)))
                        .collect();
                    Some(oxbase_types::Statement::destructive(format!(
                        "DELETE FROM {} WHERE {}",
                        q.qualify(&key.table),
                        cond.join(" AND ")
                    )))
                }
                _ => None,
            })
            .collect()
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
    assert_eq!(
        ddl.create(&def).to_string(),
        "CREATE TABLE \"shop\".\"public\".\"row 0\";\n"
    );
    assert_eq!(
        ddl.alter(&def, &newer).to_string(),
        "ALTER TABLE \"shop\".\"public\".\"row 0\" ADD COLUMN \"note\";\n"
    );
    // Callable straight on the `Arc<dyn DdlGenerator>` (no clash with `Drop::drop`).
    assert!(ddl.drop_object(&tables[0].obj).is_destructive());

    // Capabilities gate extras by version.
    let caps = reg.capabilities(Engine::Postgres).unwrap();
    assert!(caps.supports(ObjectKind::Table, props::UNLOGGED, conn.server_version()));
    assert!(!caps.supports(
        ObjectKind::Table,
        props::UNLOGGED,
        &ServerVersion::new(9, 0, 0)
    ));
    assert!(!caps.supports_kind(ObjectKind::Event, conn.server_version()));

    // DmlWriter.
    let key = RowKey::new(
        ObjectRef::table(None, Some("public"), "t"),
        oxbase_types::KeyKind::Primary,
        vec!["id".into()],
    );
    let edit = RowEdit::Delete {
        key: vec![Value::Int(7)],
    };
    key.validate(&edit).unwrap();
    let script = reg
        .dml_writer(Engine::Postgres)
        .unwrap()
        .apply(&[edit], &key);
    assert_eq!(
        script.to_string(),
        "DELETE FROM \"public\".\"t\" WHERE \"id\" = 7;\n"
    );
    assert_eq!(
        reg.quoting(Engine::Postgres).unwrap().quote_ident("x"),
        "\"x\""
    );
    assert_eq!(driver.log(), ["SELECT name FROM tables"]);
}
