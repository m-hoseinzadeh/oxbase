//! Live tests of the table designer against the Docker fixtures (PostgreSQL, MySQL, MariaDB,
//! SQL Server since M4) and a SQLite file: create two scratch tables through the designer, then add a column, an
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
use oxbase_dialect::compare::{DefaultRules, RulesComparator};
use oxbase_dialect::{Capabilities, DdlContext, DdlGenerator, Quoting, Support};
use oxbase_types::{
    CheckDef, ColumnDef, DataType, Extras, FkAction, ForeignKeyDef, IndexColumn, IndexDef,
    PrimaryKeyDef, PropId,
};

use super::*;
use crate::edit::live_tests::{ENGINES, Live, report, require_m3_cells};

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
        report(&format!(
            "{key}: designer round trip PENDING (not tested): {}",
            missing.join(", ")
        ));
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
    live.sql(&add_column_sql(live.engine, &q, "other_col int"))
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

/// `ALTER TABLE` adding `column` (T-SQL has no `COLUMN` keyword there).
fn add_column_sql(engine: Engine, qualified: &str, column: &str) -> String {
    if engine == Engine::MsSql {
        format!("ALTER TABLE {qualified} ADD {column}")
    } else {
        format!("ALTER TABLE {qualified} ADD COLUMN {column}")
    }
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
    mssql_designer_round_trip => ("mssql", Engine::MsSql);
}

#[test]
fn live_engines_are_the_m4_sql_engines() {
    let engines: Vec<Engine> = ENGINES.iter().map(|(_, e)| *e).collect();
    assert_eq!(
        engines,
        [
            Engine::Postgres,
            Engine::MySql,
            Engine::MariaDb,
            Engine::Sqlite,
            Engine::MsSql
        ]
    );
    assert_eq!(
        add_column_sql(Engine::MsSql, "[dbo].[t]", "c int"),
        "ALTER TABLE [dbo].[t] ADD c int"
    );
}

// ---------------------------------------------------------------------------------------------
// The service's apply path on engines whose DDL cells have not landed yet
// ---------------------------------------------------------------------------------------------

/// A minimal standard-SQL generator for the live apply test (only while the engine has none of
/// its own): `CREATE TABLE` and `ALTER TABLE ... ADD COLUMN`. A new column named `boom` adds a
/// step that fails on every engine (it adds the first column again).
struct GenericDdl {
    quoting: Arc<dyn Quoting>,
    transactional: bool,
}

impl GenericDdl {
    fn type_sql(data_type: &DataType) -> String {
        match data_type {
            DataType::Int { .. } => "integer".to_owned(),
            DataType::VarChar { length: Some(n) } => format!("varchar({n})"),
            _ => "varchar(255)".to_owned(),
        }
    }

    fn column_sql(&self, column: &ColumnDef) -> String {
        let not_null = if column.nullable { "" } else { " NOT NULL" };
        format!(
            "{} {}{not_null}",
            self.quoting.quote_ident(&column.name),
            Self::type_sql(&column.data_type)
        )
    }

    fn step(&self, sql: String, description: &str) -> ScriptStep {
        let step = ScriptStep::new(sql).described(description);
        if self.transactional {
            step
        } else {
            step.non_transactional()
        }
    }
}

impl DdlGenerator for GenericDdl {
    fn create(&self, d: &ObjectDef, _ctx: &DdlContext<'_>) -> Result<Script> {
        let ObjectDef::Table(t) = d else {
            return Err(Error::unsupported("only tables"));
        };
        let mut parts: Vec<String> = t.columns.iter().map(|c| self.column_sql(c)).collect();
        if let Some(pk) = &t.primary_key {
            let columns: Vec<String> = pk
                .columns
                .iter()
                .map(|c| self.quoting.quote_ident(c))
                .collect();
            parts.push(format!("PRIMARY KEY ({})", columns.join(", ")));
        }
        let sql = format!(
            "CREATE TABLE {} ({})",
            self.quoting.qualify(&t.obj),
            parts.join(", ")
        );
        Ok([self.step(sql, "Create the table")].into_iter().collect())
    }

    fn alter(&self, old: &ObjectDef, new: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        let (ObjectDef::Table(a), ObjectDef::Table(b)) = (old, new) else {
            return Err(Error::unsupported("only tables"));
        };
        if !ctx.renames.is_empty() || a.columns.iter().any(|c| b.column(&c.name).is_none()) {
            return Err(Error::unsupported("the test generator only adds columns"));
        }
        let table = self.quoting.qualify(&b.obj);
        let mut script = Script::new();
        for column in b.columns.iter().filter(|c| a.column(&c.name).is_none()) {
            let sql = if column.name == "boom" {
                format!(
                    "ALTER TABLE {table} ADD COLUMN {}",
                    self.column_sql(&a.columns[0])
                )
            } else {
                format!("ALTER TABLE {table} ADD COLUMN {}", self.column_sql(column))
            };
            script.push_step(self.step(sql, "Add a column"));
        }
        Ok(script)
    }

    fn drop_object(&self, r: &ObjectRef, _ctx: &DdlContext<'_>) -> Result<Script> {
        Ok(Script::single(format!(
            "DROP TABLE {}",
            self.quoting.qualify(r)
        )))
    }
}

/// The engine's capabilities with a designer that can create tables and add columns.
struct AddColumnsOnly {
    inner: Arc<dyn Capabilities>,
    transactional: bool,
}

impl Capabilities for AddColumnsOnly {
    fn supports(&self, kind: ObjectKind, prop: PropId, v: &ServerVersion) -> bool {
        self.inner.supports(kind, prop, v)
    }

    fn supports_kind(&self, kind: ObjectKind, v: &ServerVersion) -> bool {
        self.inner.supports_kind(kind, v)
    }

    fn designer(&self, _v: &ServerVersion) -> DesignerCapabilities {
        DesignerCapabilities {
            transactional_ddl: self.transactional,
            create_table: true,
            add_column: Support::Native,
            auto_increment: true,
            identity: true,
            unique_constraints: true,
            checks_enforced: true,
            max_identifier_length: Some(63),
            ..DesignerCapabilities::default()
        }
    }
}

async fn generic_apply(key: &'static str, engine: Engine) {
    if crate::default_dialects()
        .ddl(engine, ObjectKind::Table)
        .is_ok()
    {
        report(&format!(
            "{key}: the engine has its own DDL cells; its round trip covers the apply"
        ));
        return;
    }
    // MySQL and MariaDB commit every DDL statement on their own.
    let transactional = !matches!(engine, Engine::MySql | Engine::MariaDb);
    let live = Live::connect_with(key, engine, |registry| {
        let quoting = registry.quoting(engine).unwrap();
        let inner = registry.capabilities(engine).unwrap();
        registry
            .register_ddl(
                engine,
                ObjectKind::Table,
                Arc::new(GenericDdl {
                    quoting,
                    transactional,
                }),
            )
            .register_comparator(
                engine,
                ObjectKind::Table,
                Arc::new(RulesComparator(DefaultRules)),
            )
            .register_capabilities(
                engine,
                Arc::new(AddColumnsOnly {
                    inner,
                    transactional,
                }),
            );
    })
    .await;
    let table = live.table(&format!("m3_svc_{}_g", live.token));
    live.drop_table(&table).await;
    let q = live.qualified(&table);
    live.sql(&format!(
        "CREATE TABLE {q} (id integer NOT NULL PRIMARY KEY, name varchar(20))"
    ))
    .await;
    let outcome = std::panic::AssertUnwindSafe(generic_steps(&live, &table, transactional));
    let result = futures::FutureExt::catch_unwind(outcome).await;
    live.drop_table(&table).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    report(&format!(
        "{key}: designer apply (generic generator, transactional {transactional}) ok"
    ));
}

async fn generic_steps(live: &Live, table: &ObjectRef, transactional: bool) {
    let key = live.key;
    let names = |design: &TableDesign| -> Vec<String> {
        design
            .columns
            .iter()
            .map(|c| c.column.name.clone())
            .collect()
    };
    let load = || async {
        live.designer
            .load(live.session, table, CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{key}: load: {e}"))
    };
    let options = DesignApplyOptions::default;

    // Add a column: one step, committed, reloaded.
    let loaded = load().await;
    let mut edited = loaded.clone();
    edited
        .columns
        .push(column("note", DataType::VarChar { length: Some(20) }, true));
    let applied = live
        .designer
        .apply(live.session, &edited, options(), CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("{key}: apply: {e}"));
    assert!(applied.outcome.committed, "{key}: {:?}", applied.outcome);
    assert_eq!(applied.outcome.atomic, transactional, "{key}");
    let reloaded = applied.design.expect("reloaded");
    assert_eq!(names(&reloaded), ["id", "name", "note"], "{key}");

    // A failing second step: all or nothing where DDL is transactional.
    let mut edited = reloaded.clone();
    edited
        .columns
        .push(column("late", DataType::VarChar { length: Some(20) }, true));
    edited
        .columns
        .push(column("boom", DataType::VarChar { length: Some(20) }, true));
    let failed = live
        .designer
        .apply(live.session, &edited, options(), CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("{key}: apply: {e}"));
    assert!(!failed.outcome.committed, "{key}: {:?}", failed.outcome);
    let statuses: Vec<StepStatus> = failed.outcome.steps.iter().map(|s| s.status).collect();
    if transactional {
        assert_eq!(
            statuses,
            [StepStatus::RolledBack, StepStatus::Failed],
            "{key}"
        );
        assert_eq!(failed.design, None, "{key}");
        assert_eq!(names(&load().await), ["id", "name", "note"], "{key}");
    } else {
        assert_eq!(statuses, [StepStatus::Done, StepStatus::Failed], "{key}");
        let partial = failed.design.expect("a non-atomic failure reloads");
        assert_eq!(names(&partial), ["id", "name", "note", "late"], "{key}");
    }

    // The table changes between load and apply: refused, nothing runs.
    let before = load().await;
    let mut edited = before.clone();
    edited.columns.push(column(
        "later",
        DataType::VarChar { length: Some(20) },
        true,
    ));
    let q = live.qualified(table);
    live.sql(&add_column_sql(live.engine, &q, "other_col integer"))
        .await;
    let err = live
        .designer
        .apply(live.session, &edited, options(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains(CHANGED_SINCE_LOADED),
        "{key}: {err}"
    );
    assert!(
        load()
            .await
            .columns
            .iter()
            .all(|c| c.column.name != "later"),
        "{key}"
    );
}

macro_rules! generic_per_engine {
    ($($name:ident => ($key:literal, $engine:expr);)*) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            #[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
            async fn $name() {
                generic_apply($key, $engine).await;
            }
        )*
    };
}

generic_per_engine! {
    postgres_designer_apply_path => ("postgres", Engine::Postgres);
    mysql_designer_apply_path => ("mysql", Engine::MySql);
    mariadb_designer_apply_path => ("mariadb", Engine::MariaDb);
    sqlite_designer_apply_path => ("sqlite", Engine::Sqlite);
}
