//! Live tests of the table designer against the Docker fixtures (PostgreSQL, MySQL, MariaDB)
//! and a SQLite file: create two scratch tables through the designer, then add a column, an
//! index, a foreign key and a check; the preview text, the apply, and the reloaded design equal
//! to the edited model (the engine's comparator finds no difference); and the conflict when the
//! table changed between load and apply.
//!
//! The designer needs the engine's `DdlGenerator`, `Comparator` and designer capabilities
//! (engine streams). Until an engine registers them the test reports it as pending and passes;
//! with `OXBASE_SERVICES_REQUIRE_M3_CELLS=1` a pending engine fails.
//!
//! ```sh
//! scripts/fixtures-up.sh
//! cargo test -p oxbase-services --lib designer::live_tests -- --ignored --test-threads 1
//! ```

use oxbase_designer::{DesignColumn, DesignItem};
use oxbase_types::{
    CheckDef, ColumnDef, DataType, Extras, FkAction, ForeignKeyDef, IndexColumn, IndexDef,
    PrimaryKeyDef,
};

use super::*;
use crate::edit::live_tests::{ENGINES, Live, report};

/// `true` when a pending engine must fail the test.
pub(crate) fn require_m3_cells() -> bool {
    std::env::var("OXBASE_SERVICES_REQUIRE_M3_CELLS").is_ok_and(|v| v == "1")
}

fn column(name: &str, data_type: DataType, nullable: bool) -> DesignColumn {
    let mut column = ColumnDef::new(name, data_type);
    column.nullable = nullable;
    DesignColumn {
        origin: None,
        column,
        default_literal: None,
    }
}

fn index(name: String, column: &str) -> DesignItem<IndexDef> {
    DesignItem::new(IndexDef {
        name,
        columns: vec![IndexColumn::column(column)],
        unique: false,
        is_constraint: false,
        predicate: None,
        extras: Extras::new(),
    })
}

async fn apply(live: &Live, design: &TableDesign, step: &str) -> TableDesign {
    let preview = live
        .designer
        .preview(live.session, design, CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("{}: {step}: preview: {e}", live.key));
    assert!(
        !preview.has_errors() && !preview.script.is_empty(),
        "{}: {step}: {:?}",
        live.key,
        preview.issues
    );
    assert!(!preview.sql.trim().is_empty(), "{}: {step}", live.key);
    report(&format!(
        "{}: {step}: atomic {}, destructive {}\n{}",
        live.key, preview.atomic, preview.destructive, preview.sql
    ));
    let applied = live
        .designer
        .apply(
            live.session,
            design,
            DesignApplyOptions {
                confirm_destructive: true,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("{}: {step}: apply: {e}", live.key));
    assert!(
        applied.outcome.committed,
        "{}: {step}: {:?}",
        live.key, applied.outcome
    );
    let reloaded = applied
        .design
        .unwrap_or_else(|| panic!("{}: {step}: no reloaded design", live.key));
    // The table as reloaded equals the model it was generated from.
    let quoting = live.dialects.quoting(live.engine).unwrap();
    let comparator = live
        .dialects
        .comparator(live.engine, ObjectKind::Table)
        .unwrap();
    let diffs = comparator.diff(
        &ObjectDef::Table(design.to_def(quoting.as_ref())),
        &ObjectDef::Table(reloaded.original.clone().unwrap()),
    );
    assert!(
        diffs.is_empty(),
        "{}: {step}: the reloaded table differs: {}",
        live.key,
        diffs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    );
    reloaded
}

async fn designer_cycle(key: &'static str, engine: Engine) {
    let live = Live::connect(key, engine, false).await;
    let missing = live.dialects.missing_designer_roles(engine);
    if !missing.is_empty() {
        report(&format!("{key}: designer pending: {}", missing.join(", ")));
        assert!(
            !require_m3_cells(),
            "{key}: designer cells missing: {missing:?}"
        );
        return;
    }
    let base = format!("m3_svc_{}_d", live.token);
    let parent = live.table(&format!("{base}ref"));
    let child = live.table(&base);
    live.drop_table(&child).await;
    live.drop_table(&parent).await;
    let outcome = std::panic::AssertUnwindSafe(designer_steps(&live, &parent, &child));
    let result = futures::FutureExt::catch_unwind(outcome).await;
    live.drop_table(&child).await;
    live.drop_table(&parent).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    report(&format!(
        "{key}: designer create / alter / reload / conflict ok"
    ));
    // The manager closes every session at the end.
    live.manager.close_all().await.unwrap();
}

async fn designer_steps(live: &Live, parent: &ObjectRef, child: &ObjectRef) {
    // New tables.
    let mut design = TableDesign::new_table(parent.clone());
    design.columns.push(column("id", DataType::INT32, false));
    design.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec!["id".into()],
    });
    apply(live, &design, "create the referenced table").await;
    let mut design = TableDesign::new_table(child.clone());
    design.columns.push(column("id", DataType::INT32, false));
    design
        .columns
        .push(column("name", DataType::VarChar { length: Some(50) }, true));
    design.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec!["id".into()],
    });
    let loaded = apply(live, &design, "create the table").await;
    assert_eq!(loaded.columns.len(), 2, "{}", live.key);

    // Add a column, indexes, a foreign key and a check.
    let mut edited = loaded.clone();
    edited
        .columns
        .push(column("parent_id", DataType::INT32, true));
    let mut qty = column("qty", DataType::INT32, false);
    qty.column.default = Some("0".into());
    edited.columns.push(qty);
    let prefix = format!("m3s{}", live.token);
    edited
        .indexes
        .push(index(format!("{prefix}_name_idx"), "name"));
    edited
        .indexes
        .push(index(format!("{prefix}_parent_idx"), "parent_id"));
    edited.foreign_keys.push(DesignItem::new(ForeignKeyDef {
        name: Some(format!("{prefix}_fk")),
        columns: vec!["parent_id".into()],
        ref_table: parent.clone(),
        ref_columns: vec!["id".into()],
        on_delete: FkAction::Cascade,
        on_update: FkAction::NoAction,
        extras: Extras::new(),
    }));
    edited.checks.push(DesignItem::new(CheckDef {
        name: Some(format!("{prefix}_ck")),
        expression: "qty >= 0".into(),
        extras: Extras::new(),
    }));
    let preview = live
        .designer
        .preview(live.session, &edited, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        preview.sql.contains("parent_id"),
        "{}: {}",
        live.key,
        preview.sql
    );
    assert!(!preview.diffs.is_empty(), "{}", live.key);
    let reloaded = apply(live, &edited, "add column, indexes, foreign key and check").await;
    assert_eq!(reloaded.columns.len(), 4, "{}", live.key);
    assert_eq!(reloaded.foreign_keys.len(), 1, "{}", live.key);
    assert_eq!(reloaded.checks.len(), 1, "{}", live.key);

    // Someone else changes the table between load and apply: refused, nothing runs.
    let mut late = reloaded.clone();
    late.columns.push(column("late", DataType::INT32, true));
    let q = live.qualified(child);
    live.sql(&format!("ALTER TABLE {q} ADD COLUMN other_col int"))
        .await;
    let err = live
        .designer
        .apply(
            live.session,
            &late,
            DesignApplyOptions {
                confirm_destructive: true,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput, "{}", live.key);
    assert!(
        err.to_string().contains(CHANGED_SINCE_LOADED),
        "{}: {err}",
        live.key
    );
    let current = live
        .designer
        .load(live.session, child, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        current.columns.iter().all(|c| c.column.name != "late"),
        "{}",
        live.key
    );
}

macro_rules! per_engine {
    ($($name:ident => ($key:literal, $engine:expr);)*) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            #[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
            async fn $name() {
                designer_cycle($key, $engine).await;
            }
        )*
    };
}

per_engine! {
    postgres_designer_round_trip => ("postgres", Engine::Postgres);
    mysql_designer_round_trip => ("mysql", Engine::MySql);
    mariadb_designer_round_trip => ("mariadb", Engine::MariaDb);
    sqlite_designer_round_trip => ("sqlite", Engine::Sqlite);
}

#[test]
fn live_engines_are_the_m3_sql_engines() {
    let engines: Vec<Engine> = ENGINES.iter().map(|(_, e)| *e).collect();
    assert_eq!(
        engines,
        [
            Engine::Postgres,
            Engine::MySql,
            Engine::MariaDb,
            Engine::Sqlite
        ]
    );
}
