//! Unit tests of the round-trip harness. The end-to-end ones run the real SQLite driver and
//! the real SQLite fetchers (`oxbase_services::default_dialects`) with a toy `DdlGenerator`
//! (rebuild-everything, test-only) and the default comparator rules, so the harness itself is
//! proven before any engine registers its generator.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use oxbase_dialect::compare::{CompareRules, DefaultRules, RulesComparator, diff_tables};
use oxbase_dialect::{
    AnsiQuoting, Capabilities, Comparator, DdlContext, DdlGenerator, DesignerCapabilities, Quoting,
    Support,
};
use oxbase_driver::CancellationToken;
use oxbase_services::{ProvidedSecrets, SessionHandle};
use oxbase_store::ProfileAddress;
use oxbase_types::{
    ColumnDef, DataType, Error, FkAction, ObjectDef, PropId, Result, Script, ScriptStep,
    ServerVersion, StepPhase, TableDef,
};
use pretty_assertions::assert_eq;

use super::cells::{base_table, child_of, may_skip, with_child};
use super::*;
use crate::matrix::plan_engine_in;

// --- names and tokens ----------------------------------------------------------------------

#[test]
fn tokens_are_ten_base36_chars_with_readable_seconds() {
    let t = token(1_793_000_000, 4_194_303);
    assert_eq!(t.len(), TOKEN_LEN);
    assert!(is_token(&t), "{t}");
    assert_eq!(token_secs(&t), Some(1_793_000_000));
    assert_eq!(token(0, 0), "0000000000");
    // Only the low digits of a large pid are kept.
    assert_eq!(&token(0, u32::MAX)[..6], "000000");
    let fresh = new_token();
    assert!(is_token(&fresh), "{fresh}");
    assert_ne!(fresh, new_token());
    assert_eq!(token_secs("short"), None);
    assert_eq!(token_secs("ABCDEFGHIJ"), None);
}

#[test]
fn token_ages_survive_the_wrap_of_the_seconds_field() {
    assert_eq!(SECS_WRAP, 36u64.pow(u32::try_from(SECS_DIGITS).unwrap()));
    // The seconds field wraps on 2038-12-24.
    let wrap = SECS_WRAP;
    let now = wrap + 100;
    // Made 50 s ago, after the wrap: young, not ~69 years old.
    assert_eq!(token_age(&token(wrap + 50, 1), now), Some(50));
    // Made before the wrap.
    assert_eq!(token_age(&token(wrap - 10, 1), now), Some(110));
    // From another machine whose clock is ahead: never stale.
    assert_eq!(token_age(&token(now + 30, 1), now), None);
    assert_eq!(token_age(&token(1_000, 1), 4_600), Some(3_600));
    assert_eq!(token_age("short", now), None);
}

#[test]
fn gate_options_never_let_skipping_pass_the_required_gate() {
    let mut opts = RunOptions::default();
    let plain = GateOptions::for_run(&opts, false).unwrap();
    assert!(!plain.skipped_ok);
    opts.skip_roundtrip = true;
    assert!(GateOptions::for_run(&opts, false).unwrap().skipped_ok);
    let err = GateOptions::for_run(&opts, true).unwrap_err();
    assert!(err.contains("both set"), "{err}");
    opts.skip_roundtrip = false;
    opts.allow_missing = true;
    let required = GateOptions::for_run(&opts, true).unwrap();
    assert!(required.skipped_ok && required.require_m3, "{required:?}");
}

#[test]
fn only_the_contract_s_exceptions_may_be_skipped() {
    let v = |s: &str| ServerVersion::parse(s).unwrap();
    assert!(may_skip(
        Engine::Sqlite,
        &v("3.45.1"),
        "change table comment"
    ));
    assert!(may_skip(Engine::Sqlite, &v("3.45.1"), "set column comment"));
    assert!(!may_skip(Engine::Sqlite, &v("3.45.1"), "rename column"));
    assert!(!may_skip(
        Engine::Postgres,
        &v("16.2"),
        "set column comment"
    ));
    assert!(may_skip(Engine::MySql, &v("8.0.15"), "drop check"));
    assert!(!may_skip(Engine::MySql, &v("8.0.16"), "drop check"));
    assert!(!may_skip(Engine::MySql, &v("8.4.0"), "rename table"));
    assert!(!may_skip(Engine::MariaDb, &v("11.4.2"), "change check"));
}

#[test]
fn scratch_names_carry_the_token() {
    let names = table_names("m3_rt_", "abc123defg", ObjectKind::ForeignKey);
    assert_eq!(
        names,
        vec!["m3_rt_foreign_key_abc123defg", "m3_rt_fk_abc123defg_ref"]
    );
    assert_eq!(
        table_names("m3_rt_", "abc123defg", ObjectKind::Table),
        vec![
            "m3_rt_table_abc123defg",
            "m3_rt_table_abc123defg_rn",
            "m3_rt_table_abc123defg_tx"
        ]
    );
    for kind in DESIGNER_KINDS {
        for name in table_names("m3_rt_", "abc123defg", kind) {
            assert_eq!(scratch_token(&name, "m3_rt_"), Some("abc123defg"), "{name}");
            // Every name fits PostgreSQL's 63-byte limit with the suffixes the cells add.
            assert!(name.len() + 3 <= 63, "{name}");
        }
    }
    assert_eq!(scratch_token("m3_edit_pk", "m3_rt_"), None);
    assert_eq!(scratch_token("m3_rt_table_notatoken", "m3_rt_"), None);
    // Reverse name order drops a referencing table before the table it references.
    let mut fk = table_names("m3_rt_", "abc123defg", ObjectKind::ForeignKey);
    fk.sort_by(|a, b| b.cmp(a));
    assert_eq!(fk[0], "m3_rt_foreign_key_abc123defg");
}

#[test]
fn missing_roles_name_what_a_cell_needs() {
    let reg = adapter::provisional_registry();
    assert_eq!(
        missing_roles(&reg, Engine::Sqlite, ObjectKind::Index),
        vec![
            "table DdlGenerator",
            "table Comparator",
            "table Fetcher",
            "index DdlGenerator",
            "index Comparator",
            "index Fetcher",
            "Quoting",
            "Capabilities"
        ]
    );
    assert_eq!(
        runnable_kinds(&reg, Engine::Sqlite),
        Vec::<ObjectKind>::new()
    );
    let toy = toy_registry(ToyDdl::default(), toy_caps());
    assert_eq!(
        missing_roles(&toy, Engine::Sqlite, ObjectKind::Check),
        Vec::<String>::new()
    );
    assert_eq!(
        runnable_kinds(&toy, Engine::Sqlite),
        DESIGNER_KINDS.to_vec()
    );
    // No driver yet: nothing runs, whatever is registered.
    assert_eq!(
        runnable_kinds(&toy, Engine::MsSql),
        Vec::<ObjectKind>::new()
    );
    assert_eq!(
        pending_detail(&toy, Engine::Sqlite, ObjectKind::Check, Check::Roundtrip),
        "pending (roles registered; not run)"
    );
}

#[test]
fn with_child_adds_replaces_and_removes() {
    let t = base_table(ObjectRef::table(Some("main"), None, "t"));
    let col = ObjectDef::Column {
        table: t.obj.clone(),
        column: ColumnDef::new("extra", DataType::Text),
    };
    let added = with_child(&t, ObjectKind::Column, None, Some(&col));
    assert_eq!(added.columns.len(), 5);
    assert_eq!(added.columns[3].name, "order");
    assert_eq!(added.columns[4].name, "extra");
    let mut renamed = col.clone();
    if let ObjectDef::Column { column, .. } = &mut renamed {
        "other".clone_into(&mut column.name);
    }
    let changed = with_child(&added, ObjectKind::Column, Some("extra"), Some(&renamed));
    assert_eq!(changed.columns[4].name, "other");
    let removed = with_child(&changed, ObjectKind::Column, Some("other"), None);
    assert_eq!(removed, t);
    assert!(child_of(&changed, ObjectKind::Column, "other").is_some());
    assert!(child_of(&changed, ObjectKind::Index, "other").is_none());
}

// --- gate ----------------------------------------------------------------------------------

fn engine(name: &str, engine: Engine) -> ResolvedEngine {
    ResolvedEngine {
        name: name.to_owned(),
        engine,
        target: crate::probe::Target::File {
            path: "x.sqlite".into(),
        },
        version_prefix: None,
    }
}

fn set(cells: &mut [Cell], kind: ObjectKind, check: Check, status: CellStatus) {
    let c = cells
        .iter_mut()
        .find(|c| c.object_kind == kind && c.check == check)
        .unwrap();
    c.status = status;
    c.detail = Some("set by the test".to_owned());
}

#[test]
fn gate_accepts_pending_unless_m3_is_required() {
    let reg = adapter::provisional_registry();
    let fx = Fixtures::parse("schema_version = 1\n", "t").unwrap();
    let engines = vec![engine("lite", Engine::Sqlite)];
    let mut cells = plan_engine_in(&reg, "lite", Engine::Sqlite);
    assert_eq!(
        gate(&cells, &fx, &engines, &reg, GateOptions::default()),
        Vec::<String>::new()
    );

    let required = GateOptions {
        skipped_ok: false,
        require_m3: true,
    };
    let problems = gate(&cells, &fx, &engines, &reg, required);
    assert!(
        problems[0].starts_with("lite: missing designer roles"),
        "{problems:#?}"
    );
    assert!(
        problems.iter().any(|p| p.contains("has no [m3.lite]")),
        "{problems:#?}"
    );
    // Five kinds, two checks.
    assert_eq!(
        problems.iter().filter(|p| p.contains("must pass")).count(),
        10,
        "{problems:#?}"
    );

    for kind in DESIGNER_KINDS {
        set(&mut cells, kind, Check::Roundtrip, CellStatus::Pass);
        set(&mut cells, kind, Check::Capabilities, CellStatus::Pass);
    }
    let problems = gate(&cells, &fx, &engines, &reg, required);
    assert!(
        !problems.iter().any(|p| p.contains("must pass")),
        "{problems:#?}"
    );

    // A failure always counts; so does a pass where no round trip exists.
    set(
        &mut cells,
        ObjectKind::Index,
        Check::Roundtrip,
        CellStatus::Fail,
    );
    set(
        &mut cells,
        ObjectKind::View,
        Check::Roundtrip,
        CellStatus::Pass,
    );
    let problems = gate(&cells, &fx, &engines, &reg, GateOptions::default());
    assert_eq!(problems.len(), 2, "{problems:#?}");
    assert!(
        problems[0].starts_with("lite index roundtrip: fail"),
        "{problems:#?}"
    );
    assert!(
        problems[1].starts_with("lite view roundtrip: pass"),
        "{problems:#?}"
    );
}

// --- the executor on a real SQLite session -------------------------------------------------

/// A fixtures file with one SQLite fixture `name` and an `[m3]` table.
fn sqlite_fixture(name: &str) -> (Fixtures, Vec<ResolvedEngine>) {
    let text = format!(
        "schema_version = 1
[engines.{name}]
kind = \"sqlite\"
path = \"target/conformance/{name}-unused.sqlite\"
[m3]
roundtrip_prefix = \"m3_rt_\"
roundtrip_kinds = [\"table\", \"column\", \"index\", \"foreign_key\", \"check\"]
stale_after_secs = 3600
[m3.{name}]
database = \"main\"
"
    );
    let fx = Fixtures::parse(&text, name).unwrap();
    let engines = fx.resolve().unwrap();
    (fx, engines)
}

async fn sqlite_session(dir: &Path) -> (Ctx, SessionHandle) {
    let (fx, engines) = sqlite_fixture("lite_exec");
    let ctx = Ctx::new(&fx);
    let mut p = ctx.profile(&engines[0], false).await.unwrap();
    p.address = ProfileAddress::File {
        path: dir.join("exec.sqlite"),
    };
    let s = ctx
        .mgr
        .open(&p, &ProvidedSecrets::none(), CancellationToken::new())
        .await
        .unwrap();
    (ctx, s)
}

async fn values(conn: &dyn Connection, cancel: &CancellationToken) -> Option<String> {
    let out = exec(
        conn,
        "SELECT group_concat(a, ',') FROM (SELECT a FROM t ORDER BY a)",
        &[],
        cancel,
    )
    .await
    .unwrap();
    first_text(&out)
}

#[tokio::test]
async fn run_script_follows_phases_captures_and_checks() {
    let tmp = tempfile::tempdir().unwrap();
    let (_ctx, s) = sqlite_session(tmp.path()).await;
    let conn = s.connection().as_ref();
    let cancel = CancellationToken::new();
    exec(conn, "CREATE TABLE t (a INTEGER)", &[], &cancel)
        .await
        .unwrap();

    // Atomic: the failing check rolls the main steps back; finally steps still run, by
    // condition.
    let script: Script = [
        ScriptStep::new("SELECT 'x'")
            .in_phase(StepPhase::Prepare)
            .capture("v"),
        ScriptStep::new("INSERT INTO t VALUES (1)"),
        ScriptStep::new("SELECT a FROM t").expect_no_rows(),
        ScriptStep::new("INSERT INTO t VALUES (2)")
            .in_phase(StepPhase::Finally)
            .only_if("v", "x"),
        ScriptStep::new("INSERT INTO t VALUES (3)")
            .in_phase(StepPhase::Finally)
            .only_if("v", "y"),
    ]
    .into_iter()
    .collect();
    let err = run_script(conn, &script, true, &cancel).await.unwrap_err();
    assert!(err.contains("expected no rows, got 1"), "{err}");
    assert_eq!(values(conn, &cancel).await.as_deref(), Some("2"));

    // Not atomic: the step before the failure stays, the one after does not run.
    let script: Script = [
        ScriptStep::new("INSERT INTO t VALUES (5)").non_transactional(),
        ScriptStep::new("INSERT INTO missing VALUES (1)")
            .non_transactional()
            .described("broken"),
        ScriptStep::new("INSERT INTO t VALUES (6)").non_transactional(),
    ]
    .into_iter()
    .collect();
    let err = run_script(conn, &script, false, &cancel).await.unwrap_err();
    assert!(err.starts_with("step \"broken\""), "{err}");
    assert_eq!(values(conn, &cancel).await.as_deref(), Some("2,5"));

    // Atomic success commits; a failing finally step is reported.
    let script: Script = [
        ScriptStep::new("INSERT INTO t VALUES (7)"),
        ScriptStep::new("SELECT a FROM t WHERE a > 100").expect_no_rows(),
        ScriptStep::new("SELECT * FROM missing").in_phase(StepPhase::Finally),
    ]
    .into_iter()
    .collect();
    let err = run_script(conn, &script, true, &cancel).await.unwrap_err();
    assert!(err.starts_with("finally: "), "{err}");
    assert_eq!(values(conn, &cancel).await.as_deref(), Some("2,5,7"));
    let script = Script::single("INSERT INTO t VALUES (8)");
    run_script(conn, &script, true, &cancel).await.unwrap();
    assert_eq!(values(conn, &cancel).await.as_deref(), Some("2,5,7,8"));
}

// --- the whole round trip with a toy SQLite generator --------------------------------------

/// Test-only SQLite DDL: `CREATE TABLE` from the definition, table renames in place, and
/// every other change as a rebuild that copies the rows (create a new table, copy, drop the
/// old one, rename). No comments.
#[derive(Debug, Clone, Copy, Default)]
struct ToyDdl {
    /// Leaves column defaults out of `CREATE TABLE`: a broken generator.
    drop_defaults: bool,
    /// Rebuilds without copying the rows: a broken generator.
    lose_rows: bool,
    /// Says `Unsupported` for column renames.
    refuse_column_rename: bool,
}

const Q: AnsiQuoting = AnsiQuoting;

fn toy_type(c: &ColumnDef) -> Result<String> {
    if !c.native_type.trim().is_empty() {
        return Ok(c.native_type.clone());
    }
    match &c.data_type {
        DataType::Int { bits: 64, .. } => Ok("BIGINT".to_owned()),
        DataType::VarChar { length: Some(n) } => Ok(format!("VARCHAR({n})")),
        DataType::Text => Ok("TEXT".to_owned()),
        other => Err(Error::unsupported(format!("toy: type {other}"))),
    }
}

fn idents(names: &[String]) -> String {
    names
        .iter()
        .map(|n| Q.quote_ident(n))
        .collect::<Vec<_>>()
        .join(", ")
}

fn constraint(name: Option<&str>) -> String {
    name.map_or_else(String::new, |n| format!("CONSTRAINT {} ", Q.quote_ident(n)))
}

fn action(a: FkAction) -> &'static str {
    match a {
        FkAction::NoAction => "NO ACTION",
        FkAction::Restrict => "RESTRICT",
        FkAction::Cascade => "CASCADE",
        FkAction::SetNull => "SET NULL",
        FkAction::SetDefault => "SET DEFAULT",
    }
}

fn has_text(s: Option<&str>) -> bool {
    s.is_some_and(|c| !c.is_empty())
}

impl ToyDdl {
    fn create_table(self, t: &TableDef) -> Result<Script> {
        if has_text(t.comment.as_deref()) {
            return Err(Error::unsupported("toy: table comments"));
        }
        let mut parts = Vec::new();
        for c in &t.columns {
            if has_text(c.comment.as_deref()) {
                return Err(Error::unsupported("toy: column comments"));
            }
            let mut s = format!("{} {}", Q.quote_ident(&c.name), toy_type(c)?);
            if !c.nullable {
                s.push_str(" NOT NULL");
            }
            if let Some(d) = c.default.as_deref().filter(|_| !self.drop_defaults) {
                let _ = write!(s, " DEFAULT {d}");
            }
            parts.push(s);
        }
        if let Some(pk) = &t.primary_key {
            parts.push(format!("PRIMARY KEY ({})", idents(&pk.columns)));
        }
        for fk in &t.foreign_keys {
            parts.push(format!(
                "{}FOREIGN KEY ({}) REFERENCES {} ({}) ON DELETE {} ON UPDATE {}",
                constraint(fk.name.as_deref()),
                idents(&fk.columns),
                Q.quote_ident(&fk.ref_table.name),
                idents(&fk.ref_columns),
                action(fk.on_delete),
                action(fk.on_update)
            ));
        }
        for ck in &t.checks {
            parts.push(format!(
                "{}CHECK ({})",
                constraint(ck.name.as_deref()),
                ck.expression
            ));
        }
        let mut script = Script::new();
        script.push_step(
            ScriptStep::new(format!(
                "CREATE TABLE {} ({})",
                Q.qualify(&t.obj),
                parts.join(", ")
            ))
            .described(format!("Create table {}", t.obj.name)),
        );
        script.extend(index_steps(t));
        Ok(script)
    }

    /// Rebuilds `old` as `new`: the rows are copied (`renamed` = a column's old and new name).
    fn rebuild(
        self,
        old: &TableDef,
        new: &TableDef,
        renamed: Option<(&str, &str)>,
    ) -> Result<Script> {
        let mut tmp = new.clone();
        tmp.obj.name = format!("{}__toy_new", old.obj.name);
        tmp.indexes.clear();
        let mut s: Script = self
            .create_table(&tmp)?
            .steps
            .into_iter()
            .map(ScriptStep::destructive)
            .collect();
        let (mut to, mut from) = (Vec::new(), Vec::new());
        for c in &new.columns {
            let source = match renamed {
                Some((was, now)) if now == c.name => was,
                _ => c.name.as_str(),
            };
            if old.column(source).is_some() {
                to.push(c.name.clone());
                from.push(source.to_owned());
            }
        }
        if !self.lose_rows && !to.is_empty() {
            s.push_step(
                ScriptStep::new(format!(
                    "INSERT INTO {} ({}) SELECT {} FROM {}",
                    Q.qualify(&tmp.obj),
                    idents(&to),
                    idents(&from),
                    Q.qualify(&old.obj)
                ))
                .destructive(),
            );
        }
        s.push_step(
            ScriptStep::new(format!("DROP TABLE {}", Q.qualify(&old.obj)))
                .destructive()
                .described("Toy rebuild: drop"),
        );
        s.push_step(
            ScriptStep::new(format!(
                "ALTER TABLE {} RENAME TO {}",
                Q.qualify(&tmp.obj),
                Q.quote_ident(&new.obj.name)
            ))
            .destructive(),
        );
        s.extend(index_steps(new).into_iter().map(ScriptStep::destructive));
        Ok(s)
    }
}

/// `CREATE INDEX` of every index of `t`.
fn index_steps(t: &TableDef) -> Vec<ScriptStep> {
    let mut out = Vec::new();
    for ix in &t.indexes {
        let r =
            ObjectRef::new(ObjectKind::Index, ix.name.clone()).in_catalog(t.obj.catalog.as_deref());
        let cols: Vec<String> = ix.columns.iter().map(|c| c.name.clone()).collect();
        out.push(
            ScriptStep::new(format!(
                "CREATE {}INDEX {} ON {} ({})",
                if ix.unique { "UNIQUE " } else { "" },
                Q.qualify(&r),
                Q.quote_ident(&t.obj.name),
                idents(&cols)
            ))
            .described(format!("Create index {}", ix.name)),
        );
    }
    out
}

fn owning(ctx: &DdlContext<'_>) -> Result<TableDef> {
    ctx.table
        .cloned()
        .ok_or_else(|| Error::invalid_input("toy: no table in the context"))
}

impl DdlGenerator for ToyDdl {
    fn create(&self, d: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        match d {
            ObjectDef::Table(t) => self.create_table(t),
            child => {
                let t = owning(ctx)?;
                self.rebuild(&t, &with_child(&t, child.kind(), None, Some(child)), None)
            }
        }
    }

    fn alter(&self, old: &ObjectDef, new: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        if let (ObjectDef::Table(a), ObjectDef::Table(b)) = (old, new) {
            let mut s = Script::new();
            let mut cur = a.clone();
            if a.obj.name != b.obj.name {
                s.push_step(ScriptStep::new(format!(
                    "ALTER TABLE {} RENAME TO {}",
                    Q.qualify(&a.obj),
                    Q.quote_ident(&b.obj.name)
                )));
                cur.obj.clone_from(&b.obj);
            }
            if !diff_tables(&cur, b, &DefaultRules).is_empty() {
                s.append(self.rebuild(&cur, b, None)?);
            }
            return Ok(s);
        }
        let t = owning(ctx)?;
        let renamed = if new.kind() == ObjectKind::Column && old.name() != new.name() {
            Some((old.name(), new.name()))
        } else {
            None
        };
        if renamed.is_some() && self.refuse_column_rename {
            return Err(Error::unsupported("toy: column renames"));
        }
        self.rebuild(
            &t,
            &with_child(&t, new.kind(), Some(old.name()), Some(new)),
            renamed,
        )
    }

    fn drop_object(&self, r: &ObjectRef, ctx: &DdlContext<'_>) -> Result<Script> {
        if r.kind == ObjectKind::Table {
            let mut s = Script::new();
            s.push_destructive(format!("DROP TABLE {}", Q.qualify(r)));
            return Ok(s);
        }
        let t = owning(ctx)?;
        self.rebuild(&t, &with_child(&t, r.kind, Some(&r.name), None), None)
    }
}

/// The real SQLite capabilities with a toy designer.
struct ToyCaps {
    inner: Arc<dyn Capabilities>,
    designer: DesignerCapabilities,
}

impl Capabilities for ToyCaps {
    fn supports(&self, kind: ObjectKind, prop: PropId, v: &ServerVersion) -> bool {
        self.inner.supports(kind, prop, v)
    }

    fn supports_kind(&self, kind: ObjectKind, v: &ServerVersion) -> bool {
        self.inner.supports_kind(kind, v)
    }

    fn designer(&self, _v: &ServerVersion) -> DesignerCapabilities {
        self.designer.clone()
    }
}

/// What [`ToyDdl`] can do: renames in place, everything else by rebuild, no comments.
fn toy_caps() -> DesignerCapabilities {
    let r = Support::Rebuild;
    DesignerCapabilities {
        transactional_ddl: true,
        create_table: true,
        drop_table: true,
        rename_table: Support::Native,
        table_comment: Support::Unsupported,
        add_column: r,
        drop_column: r,
        rename_column: r,
        alter_column_type: r,
        alter_column_nullability: r,
        alter_column_default: r,
        column_comment: Support::Unsupported,
        change_primary_key: r,
        add_index: r,
        drop_index: r,
        add_foreign_key: r,
        drop_foreign_key: r,
        fk_actions: vec![
            FkAction::NoAction,
            FkAction::Restrict,
            FkAction::Cascade,
            FkAction::SetNull,
        ],
        add_check: r,
        drop_check: r,
        checks_enforced: true,
        ..DesignerCapabilities::default()
    }
}

/// `oxbase_services::default_dialects` with the toy generator, the default comparator rules
/// and `designer` for SQLite.
fn toy_registry(ddl: ToyDdl, designer: DesignerCapabilities) -> DialectRegistry {
    toy_registry_with(ddl, designer, Arc::new(RulesComparator(DefaultRules)))
}

/// [`toy_registry`] with `cmp` as the comparator of every designer kind.
fn toy_registry_with(
    ddl: ToyDdl,
    designer: DesignerCapabilities,
    cmp: Arc<dyn Comparator>,
) -> DialectRegistry {
    let mut reg = adapter::registry();
    let inner = reg.capabilities(Engine::Sqlite).unwrap();
    reg.register_capabilities(Engine::Sqlite, Arc::new(ToyCaps { inner, designer }));
    for kind in DESIGNER_KINDS {
        reg.register_ddl(Engine::Sqlite, kind, Arc::new(ddl));
        reg.register_comparator(Engine::Sqlite, kind, cmp.clone());
    }
    reg
}

/// Rules that call every two expressions (defaults, checks) the same: a comparator that
/// cannot see default or check changes.
struct BlindRules;

impl CompareRules for BlindRules {
    fn same_expression(&self, _old: &str, _new: &str) -> bool {
        true
    }
}

/// Rules that call every two foreign-key targets the same.
struct AnyTargetRules;

impl CompareRules for AnyTargetRules {
    fn same_table_ref(&self, _old: &ObjectRef, _new: &ObjectRef) -> bool {
        true
    }
}

fn find(out: &[RtOutcome], kind: ObjectKind, check: Check) -> &RtOutcome {
    out.iter()
        .find(|o| o.kind == kind && o.check == check)
        .unwrap_or_else(|| panic!("no {kind} {check} outcome in {out:#?}"))
}

#[test]
fn toy_sqlite_round_trip_passes_every_cell_and_cleans_up() {
    let (fx, engines) = sqlite_fixture("lite_toy");
    let reg = toy_registry(ToyDdl::default(), toy_caps());
    let out = run_all(&fx, &engines, &[true], &reg);
    assert_eq!(out.len(), 10, "{out:#?}");
    for o in &out {
        assert_eq!(o.status, CellStatus::Pass, "{o:#?}");
        assert!(o.elapsed_ms.is_some(), "{o:#?}");
    }
    let table = &find(&out, ObjectKind::Table, Check::Roundtrip).detail;
    assert_eq!(
        table,
        "create table, rename table, drop table [skipped, allowed on sqlite: change table \
         comment]",
        "comments are skipped on SQLite"
    );
    let caps = &find(&out, ObjectKind::Table, Check::Capabilities).detail;
    assert!(
        caps.contains("change table comment: unsupported in both"),
        "{caps}"
    );
    assert!(caps.contains("transactionalDdl = true (probed)"), "{caps}");
    let column = &find(&out, ObjectKind::Column, Check::Roundtrip).detail;
    assert!(
        column.ends_with(
            "add column, set column NOT NULL, set column default, change column type, rename \
             column, drop column [skipped, allowed on sqlite: set column comment]"
        ),
        "{column}"
    );
    let check = &find(&out, ObjectKind::Check, Check::Capabilities).detail;
    assert!(check.contains("checksEnforced = true (probed)"), "{check}");
    let fk = &find(&out, ObjectKind::ForeignKey, Check::Capabilities).detail;
    assert!(fk.contains("Cascade is in fkActions"), "{fk}");
    assert!(!scratch_dir(&engines[0]).exists());

    // The outcomes land in the matrix cells.
    let mut cells = plan_engine_in(&reg, "lite_toy", Engine::Sqlite);
    apply(&mut cells, &out);
    for kind in DESIGNER_KINDS {
        for check in [Check::Roundtrip, Check::Capabilities] {
            let c = cells
                .iter()
                .find(|c| c.object_kind == kind && c.check == check)
                .unwrap();
            assert_eq!(c.status, CellStatus::Pass, "{c:?}");
        }
    }
    let required = GateOptions {
        skipped_ok: false,
        require_m3: true,
    };
    // Every cell passes; at most the roles the toy does not provide (a DmlWriter) are left.
    for p in gate(&cells, &fx, &engines, &reg, required) {
        assert!(
            p.starts_with("lite_toy: missing designer roles")
                && p.ends_with("sqlite: no DmlWriter"),
            "{p}"
        );
    }
}

#[test]
fn a_broken_generator_fails_the_round_trip_and_still_cleans_up() {
    let (fx, engines) = sqlite_fixture("lite_broken");
    let reg = toy_registry(
        ToyDdl {
            drop_defaults: true,
            ..ToyDdl::default()
        },
        toy_caps(),
    );
    let out = run_all(&fx, &engines, &[true], &reg);
    assert_eq!(out.len(), 10, "{out:#?}");
    let table = find(&out, ObjectKind::Table, Check::Roundtrip);
    assert_eq!(table.status, CellStatus::Fail, "{table:#?}");
    assert!(
        table.detail.starts_with(
            "create table: the fetched table differs from the definition the script was \
             generated from: - column:main.m3_rt_table_"
        ),
        "{table:#?}"
    );
    assert!(table.detail.contains("qty .default"), "{table:#?}");
    assert!(!table.detail.contains("[cleanup:"), "{table:#?}");
    let caps = find(&out, ObjectKind::Table, Check::Capabilities);
    assert_eq!(caps.status, CellStatus::Pending, "{caps:#?}");
    let index = find(&out, ObjectKind::Index, Check::Roundtrip);
    assert!(
        index.detail.starts_with("create base table m3_rt_index_"),
        "{index:#?}"
    );
    assert!(!scratch_dir(&engines[0]).exists());
}

#[test]
fn a_comparator_blind_to_a_property_fails_the_round_trip() {
    let (fx, engines) = sqlite_fixture("lite_blind");
    let reg = toy_registry_with(
        ToyDdl::default(),
        toy_caps(),
        Arc::new(RulesComparator(BlindRules)),
    );
    let out = run_all(&fx, &engines, &[true], &reg);
    for kind in DESIGNER_KINDS {
        let rt = find(&out, kind, Check::Roundtrip);
        assert_eq!(rt.status, CellStatus::Fail, "{rt:#?}");
        assert!(
            rt.detail
                .contains("cannot see these changes of the fetched table: qty default 0 -> 1"),
            "{rt:#?}"
        );
    }
    assert!(!scratch_dir(&engines[0]).exists());

    let (fx, engines) = sqlite_fixture("lite_anytarget");
    let reg = toy_registry_with(
        ToyDdl::default(),
        toy_caps(),
        Arc::new(RulesComparator(AnyTargetRules)),
    );
    let out = run_all(&fx, &engines, &[true], &reg);
    let fk = find(&out, ObjectKind::ForeignKey, Check::Roundtrip);
    assert_eq!(fk.status, CellStatus::Fail, "{fk:#?}");
    assert!(
        fk.detail
            .starts_with("add foreign key: the foreign key comparator cannot see"),
        "{fk:#?}"
    );
    let column = find(&out, ObjectKind::Column, Check::Roundtrip);
    assert_eq!(column.status, CellStatus::Pass, "{column:#?}");
}

#[test]
fn a_rebuild_that_loses_rows_fails_the_round_trip() {
    let (fx, engines) = sqlite_fixture("lite_lossy");
    let reg = toy_registry(
        ToyDdl {
            lose_rows: true,
            ..ToyDdl::default()
        },
        toy_caps(),
    );
    let out = run_all(&fx, &engines, &[true], &reg);
    for kind in [
        ObjectKind::Column,
        ObjectKind::Index,
        ObjectKind::ForeignKey,
        ObjectKind::Check,
    ] {
        let rt = find(&out, kind, Check::Roundtrip);
        assert_eq!(rt.status, CellStatus::Fail, "{rt:#?}");
        assert!(
            rt.detail
                .contains("holds 0 row(s) afterwards, expected the 1 row inserted before"),
            "{rt:#?}"
        );
    }
    // Table renames do not rebuild, so the row stays.
    let table = find(&out, ObjectKind::Table, Check::Roundtrip);
    assert_eq!(table.status, CellStatus::Pass, "{table:#?}");
}

#[test]
fn a_required_change_both_sides_skip_leaves_the_cell_pending() {
    let (fx, engines) = sqlite_fixture("lite_norename");
    let mut designer = toy_caps();
    designer.rename_column = Support::Unsupported;
    let reg = toy_registry(
        ToyDdl {
            refuse_column_rename: true,
            ..ToyDdl::default()
        },
        designer,
    );
    let out = run_all(&fx, &engines, &[true], &reg);
    let rt = find(&out, ObjectKind::Column, Check::Roundtrip);
    assert_eq!(rt.status, CellStatus::Pending, "{rt:#?}");
    assert!(
        rt.detail.starts_with(
            "pending (the generator and the capabilities both call [rename column] unsupported"
        ),
        "{rt:#?}"
    );
    let caps = find(&out, ObjectKind::Column, Check::Capabilities);
    assert_eq!(caps.status, CellStatus::Pass, "{caps:#?}");

    let mut cells = plan_engine_in(&reg, "lite_norename", Engine::Sqlite);
    apply(&mut cells, &out);
    let required = GateOptions {
        skipped_ok: false,
        require_m3: true,
    };
    let problems = gate(&cells, &fx, &engines, &reg, required);
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("lite_norename column roundtrip: pending")),
        "{problems:#?}"
    );
}

#[test]
fn capabilities_that_disagree_fail_only_the_capabilities_cells() {
    let (fx, engines) = sqlite_fixture("lite_caps");
    let mut designer = toy_caps();
    // Wrong on purpose: SQLite DDL is transactional and checks are enforced.
    designer.transactional_ddl = false;
    designer.checks_enforced = false;
    let reg = toy_registry(ToyDdl::default(), designer);
    let out = run_all(&fx, &engines, &[true], &reg);
    for kind in DESIGNER_KINDS {
        let rt = find(&out, kind, Check::Roundtrip);
        assert_eq!(rt.status, CellStatus::Pass, "{rt:#?}");
        let caps = find(&out, kind, Check::Capabilities);
        assert_eq!(caps.status, CellStatus::Fail, "{caps:#?}");
        assert!(
            caps.detail
                .contains("the capabilities say DDL commits implicitly"),
            "{caps:#?}"
        );
    }
    let table = &find(&out, ObjectKind::Table, Check::Capabilities).detail;
    assert!(
        table.contains(
            "transactionalDdl = false, but a CREATE TABLE in a rolled-back transaction was undone"
        ),
        "{table}"
    );
    let check = &find(&out, ObjectKind::Check, Check::Capabilities).detail;
    assert!(
        check.contains("checksEnforced = false, but a row violating the check was rejected"),
        "{check}"
    );
}

#[test]
fn unregistered_or_unreachable_engines_produce_no_outcome() {
    let (fx, engines) = sqlite_fixture("lite_none");
    let reg = adapter::provisional_registry();
    assert_eq!(run_all(&fx, &engines, &[true], &reg), Vec::new());
    let toy = toy_registry(ToyDdl::default(), toy_caps());
    assert_eq!(run_all(&fx, &engines, &[false], &toy), Vec::new());
}

#[test]
fn without_an_m3_table_the_cells_stay_pending_with_a_reason() {
    let text = "schema_version = 1
[engines.lite_nom3]
kind = \"sqlite\"
path = \"target/conformance/lite_nom3-unused.sqlite\"
";
    let fx = Fixtures::parse(text, "t").unwrap();
    let engines = fx.resolve().unwrap();
    let toy = toy_registry(ToyDdl::default(), toy_caps());
    let out = run_all(&fx, &engines, &[true], &toy);
    assert_eq!(out.len(), 10, "{out:#?}");
    for o in &out {
        assert_eq!(o.status, CellStatus::Pending, "{o:#?}");
        assert_eq!(
            o.detail,
            "pending (roles registered; tests/fixtures.toml has no [m3.lite_nom3])"
        );
    }
}

/// Scratch tables of a killed run are dropped when the token is old enough, and only then.
#[tokio::test]
async fn stale_scratch_tables_are_dropped_by_age() {
    let tmp = tempfile::tempdir().unwrap();
    let (_ctx, s) = sqlite_session(tmp.path()).await;
    let conn = s.connection().as_ref();
    let cancel = CancellationToken::new();
    let reg = adapter::registry();
    let tables = reg.fetcher(Engine::Sqlite, ObjectKind::Table).unwrap();
    let old = token(1_000, 1);
    let fresh = new_token();
    for name in [
        format!("m3_rt_fk_{old}_ref"),
        format!("m3_rt_foreign_key_{old}"),
        format!("m3_rt_table_{fresh}"),
        "m3_edit_pk".to_owned(),
    ] {
        exec(
            conn,
            &format!("CREATE TABLE \"{name}\" (id INTEGER)"),
            &[],
            &cancel,
        )
        .await
        .unwrap();
    }
    let container = ObjectRef::database("main");
    let dropped = drop_stale(
        conn,
        &AnsiQuoting,
        tables.as_ref(),
        &container,
        "m3_rt_",
        3600,
    )
    .await
    .unwrap();
    assert_eq!(dropped, 2);
    let mut left: Vec<String> = tables
        .list(conn, &container, cancel)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.obj.name)
        .collect();
    left.sort();
    assert_eq!(
        left,
        vec!["m3_edit_pk".to_owned(), format!("m3_rt_table_{fresh}")]
    );
}
