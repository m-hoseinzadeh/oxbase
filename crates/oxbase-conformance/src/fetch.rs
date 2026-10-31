//! M2 `fetch_list` cells: for every (engine, object kind) with a registered [`Fetcher`], list
//! the seed objects of `tests/fixtures.toml` under their parent and load each one.
//!
//! The registry is `oxbase_services::default_dialects()`, the one the app uses, so a cell runs
//! as soon as its engine crate registers a fetcher there; until then it stays pending. A cell
//! passes when, for each [`SeedGroup`] of [`seeds::seed_groups`]:
//!
//! - `list(parent)` succeeds and returns only rows of the cell's kind;
//! - every seed name is listed, with the ref `parent.child(kind, name)` (routine signatures
//!   aside) and `system = false`;
//! - `load` of every listed seed row returns a definition of that kind and name whose JSON
//!   (what the webview receives) parses back to the same definition.
//!
//! A kind the live server version does not have ([`DialectRegistry::supports_kind`]) is not
//! applicable; a kind with a fetcher but no seed object stays pending.
//!
//! Server engines are seeded by their Docker init scripts (`[m2.<engine>] init_sql`). SQLite
//! has no container: the harness applies its `init_sql` to a fresh scratch file and attaches a
//! second file as `attached_as` holding `attached_table`, as `[m2.sqlite]` describes.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxbase_dialect::{DialectRegistry, Fetcher};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_services::{ProvidedSecrets, SessionHandle};
use oxbase_store::ProfileAddress;
use oxbase_types::{Engine, ObjectDef, ObjectKind, ObjectRef, ObjectSummary, Value};

use crate::adapter;
use crate::connect::{Ctx, millis};
use crate::fixtures::{Fixtures, M2EngineSeed, ResolvedEngine, in_workspace};
use crate::matrix::{CellStatus, driver_milestone};
use crate::seeds::{self, SeedGroup};

/// Upper bound for one cell (every list and load of one kind).
pub const CELL_TIMEOUT: Duration = Duration::from_secs(60);

/// The result of one `fetch_list` cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchOutcome {
    /// The fixture name (`[engines.<name>]`).
    pub engine: String,
    pub kind: ObjectKind,
    pub status: CellStatus,
    pub detail: String,
    pub elapsed_ms: Option<u64>,
}

/// The kinds whose `fetch_list` cell can run on `engine`: the engine has a driver, the kind
/// applies, and a [`Fetcher`] is registered for it.
pub fn runnable_kinds(registry: &DialectRegistry, engine: Engine) -> Vec<ObjectKind> {
    if driver_milestone(engine).is_some() {
        return Vec::new();
    }
    ObjectKind::ALL
        .into_iter()
        .filter(|&k| adapter::applies(registry, engine, k) && registry.fetcher(engine, k).is_ok())
        .collect()
}

/// The seed groups of `kind` for the fixture `e` (empty when the fixtures seed nothing).
pub fn groups_for(fixtures: &Fixtures, e: &ResolvedEngine, kind: ObjectKind) -> Vec<SeedGroup> {
    fixtures
        .m2_seed(&e.name, e.engine)
        .map(|(m2, seed)| seeds::seed_groups(m2, seed, kind))
        .unwrap_or_default()
}

/// Runs the `fetch_list` cells of every reachable engine (`reachable[i]` belongs to
/// `engines[i]`). Engines run concurrently. Cells that cannot run produce no outcome, so their
/// planned status stays.
pub fn run_all(
    fixtures: &Fixtures,
    engines: &[ResolvedEngine],
    reachable: &[bool],
    registry: &DialectRegistry,
) -> Vec<FetchOutcome> {
    let todo: Vec<&ResolvedEngine> = engines
        .iter()
        .zip(reachable)
        .filter(|&(e, &up)| up && !runnable_kinds(registry, e.engine).is_empty())
        .map(|(e, _)| e)
        .collect();
    if todo.is_empty() {
        return Vec::new();
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            return todo
                .iter()
                .flat_map(|e| {
                    failed_all(
                        e,
                        &runnable_kinds(registry, e.engine),
                        &format!("cannot start the tokio runtime: {err}"),
                    )
                })
                .collect();
        }
    };
    let registry = Arc::new(registry.clone());
    let fixtures = Arc::new(fixtures.clone());
    rt.block_on(async {
        let tasks: Vec<_> = todo
            .iter()
            .map(|&e| {
                let ctx = Ctx::new(&fixtures);
                let (registry, fixtures, e) = (registry.clone(), fixtures.clone(), e.clone());
                tokio::spawn(async move { engine_cells(&ctx, &registry, &fixtures, &e).await })
            })
            .collect();
        let mut out = Vec::new();
        for (task, e) in tasks.into_iter().zip(&todo) {
            match task.await {
                Ok(cells) => out.extend(cells),
                Err(err) => out.extend(failed_all(
                    e,
                    &runnable_kinds(&registry, e.engine),
                    &format!("fetch task panicked: {err}"),
                )),
            }
        }
        out
    })
}

fn outcome(
    e: &ResolvedEngine,
    kind: ObjectKind,
    status: CellStatus,
    detail: String,
) -> FetchOutcome {
    FetchOutcome {
        engine: e.name.clone(),
        kind,
        status,
        detail,
        elapsed_ms: None,
    }
}

fn failed_all(e: &ResolvedEngine, kinds: &[ObjectKind], why: &str) -> Vec<FetchOutcome> {
    kinds
        .iter()
        .map(|&k| outcome(e, k, CellStatus::Fail, why.to_owned()))
        .collect()
}

/// Every runnable cell of one engine, on one session.
async fn engine_cells(
    ctx: &Ctx,
    registry: &DialectRegistry,
    fixtures: &Fixtures,
    e: &ResolvedEngine,
) -> Vec<FetchOutcome> {
    let kinds = runnable_kinds(registry, e.engine);
    let seed = fixtures.m2_seed(&e.name, e.engine).map(|(_, s)| s);
    let (session, scratch) = match open_session(ctx, e, seed).await {
        Ok(s) => s,
        Err(why) => return failed_all(e, &kinds, &format!("cannot open a session: {why}")),
    };
    let version = session.info().server_version.clone();
    let mut out = Vec::with_capacity(kinds.len());
    for kind in kinds {
        let started = Instant::now();
        let groups = groups_for(fixtures, e, kind);
        let (status, detail) = if !registry.supports_kind(e.engine, kind, &version) {
            (
                CellStatus::NotApplicable,
                format!("not on server {}", version.raw()),
            )
        } else if groups.is_empty() {
            (
                CellStatus::Pending,
                format!(
                    "pending (Fetcher registered; tests/fixtures.toml [m2] seeds no {kind} on \
                     this engine)"
                ),
            )
        } else {
            match registry.fetcher(e.engine, kind) {
                Err(err) => (CellStatus::Pending, format!("pending ({err})")),
                Ok(fetcher) => {
                    let _guard = session.exclusive().await;
                    let run = check_kind(
                        fetcher.as_ref(),
                        session.connection().as_ref(),
                        kind,
                        &groups,
                    );
                    match tokio::time::timeout(CELL_TIMEOUT, run).await {
                        Ok(Ok(detail)) => (CellStatus::Pass, detail),
                        Ok(Err(detail)) => (CellStatus::Fail, detail),
                        Err(_) => (
                            CellStatus::Fail,
                            format!("did not finish within {} s", CELL_TIMEOUT.as_secs()),
                        ),
                    }
                }
            }
        };
        let mut cell = outcome(e, kind, status, detail);
        cell.elapsed_ms = Some(millis(started.elapsed()));
        out.push(cell);
    }
    let _ = ctx.mgr.close(session.id()).await;
    if let Some(dir) = scratch {
        let _ = std::fs::remove_dir_all(dir);
    }
    out
}

/// Opens a session as the fixture user. For SQLite the session is on a fresh scratch file
/// seeded by the harness; its directory is returned so the caller can delete it.
async fn open_session(
    ctx: &Ctx,
    e: &ResolvedEngine,
    seed: Option<&M2EngineSeed>,
) -> Result<(SessionHandle, Option<PathBuf>), String> {
    let mut p = ctx.profile(e, false).await?;
    if e.engine.is_networked() {
        let s = ctx
            .mgr
            .open(&p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .map_err(|err| format!("open failed: {err}"))?;
        return Ok((s, None));
    }
    if e.engine != Engine::Sqlite {
        return Err(format!("no seeding rule for the file engine {}", e.engine));
    }
    let seed = seed.ok_or_else(|| format!("no [m2.{}] in the fixtures file", e.name))?;
    let script_path = seed
        .init_sql
        .as_deref()
        .map(in_workspace)
        .ok_or_else(|| format!("[m2.{}] has no init_sql", e.name))?;
    let script = std::fs::read_to_string(&script_path)
        .map_err(|err| format!("cannot read {}: {err}", script_path.display()))?;
    let dir = std::env::temp_dir().join(format!(
        "oxbase-conformance-m2-{}-{}",
        e.name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
    let db = seed.database.as_deref().unwrap_or("main");
    p.address = ProfileAddress::File {
        path: dir.join(format!("{db}.sqlite")),
    };
    let opened: Result<SessionHandle, String> = async {
        let s = ctx
            .mgr
            .open(&p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .map_err(|err| format!("open failed: {err}"))?;
        if let Err(err) = seed_sqlite(s.connection().as_ref(), &script, seed, &dir).await {
            let _ = ctx.mgr.close(s.id()).await;
            return Err(format!("seeding {}: {err}", script_path.display()));
        }
        Ok::<_, String>(s)
    }
    .await;
    match opened {
        Ok(s) => Ok((s, Some(dir))),
        Err(err) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(err)
        }
    }
}

/// Applies the SQLite seed script and attaches the second database (`[m2.sqlite]`).
async fn seed_sqlite(
    conn: &dyn Connection,
    script: &str,
    seed: &M2EngineSeed,
    dir: &std::path::Path,
) -> Result<(), String> {
    exec(conn, script, &[]).await?;
    if let Some(alias) = seed.attached_as.as_deref() {
        let file = dir.join(format!("{alias}.sqlite"));
        exec(
            conn,
            &format!("ATTACH DATABASE ?1 AS {}", quote_ident(alias)),
            &[Value::Text(file.display().to_string())],
        )
        .await?;
        if let Some(table) = seed.attached_table.as_deref() {
            exec(
                conn,
                &format!(
                    "CREATE TABLE {}.{} (id INTEGER PRIMARY KEY)",
                    quote_ident(alias),
                    quote_ident(table)
                ),
                &[],
            )
            .await?;
        }
    }
    Ok(())
}

/// An ANSI-quoted identifier (fixture names only; never user input).
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

async fn exec(conn: &dyn Connection, sql: &str, params: &[Value]) -> Result<(), String> {
    let stream = conn
        .execute(sql, params, CancellationToken::new())
        .await
        .map_err(|err| err.to_string())?;
    oxbase_driver::collect(stream)
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// Checks one `fetch_list` cell against its seed groups (see the module docs). `Ok` carries a
/// one-line summary, `Err` the first problem.
pub async fn check_kind(
    fetcher: &dyn Fetcher,
    conn: &dyn Connection,
    kind: ObjectKind,
    groups: &[SeedGroup],
) -> Result<String, String> {
    let mut parts = Vec::with_capacity(groups.len());
    for g in groups {
        let parent = describe(&g.parent);
        let rows = fetcher
            .list(conn, &g.parent, CancellationToken::new())
            .await
            .map_err(|err| format!("list under {parent}: {err}"))?;
        if let Some(r) = rows.iter().find(|r| r.obj.kind != kind) {
            return Err(format!(
                "list of {kind} under {parent} returned {} (a {})",
                r.obj, r.obj.kind
            ));
        }
        let missing: Vec<&str> = g
            .names
            .iter()
            .filter(|n| !rows.iter().any(|r| &r.obj.name == *n))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "{} not listed under {parent} (listed: {})",
                missing.join(", "),
                preview(&rows)
            ));
        }
        let mut loaded = 0usize;
        for name in &g.names {
            let want = normalized(&g.parent.child(kind, name.clone()));
            for row in rows.iter().filter(|r| &r.obj.name == name) {
                if normalized(&row.obj) != want {
                    return Err(format!(
                        "{name} is listed as {:?}, expected {:?} (parent.child)",
                        row.obj, want
                    ));
                }
                if row.system {
                    return Err(format!("{} is flagged as a system object", row.obj));
                }
                let def = fetcher
                    .load(conn, &row.obj, CancellationToken::new())
                    .await
                    .map_err(|err| format!("load {}: {err}", row.obj))?;
                check_def(&def, kind, name).map_err(|err| format!("load {}: {err}", row.obj))?;
                loaded += 1;
            }
        }
        parts.push(format!(
            "{} under {parent}: listed, {loaded} loaded ({} rows)",
            g.names.join(", "),
            rows.len()
        ));
    }
    Ok(parts.join("; "))
}

/// A parent for messages: `the server` for the root, else the ref's display form.
fn describe(parent: &ObjectRef) -> String {
    if parent.is_root() {
        "the server".to_owned()
    } else {
        parent.to_string()
    }
}

/// A ref without its routine signature, with an unset `parent_kind` of a table child read as
/// `Table` (what [`ObjectRef::owner`] does).
fn normalized(r: &ObjectRef) -> ObjectRef {
    let mut r = r.clone();
    r.signature = None;
    if r.parent.is_some() && r.parent_kind.is_none() {
        r.parent_kind = Some(ObjectKind::Table);
    }
    r
}

/// Checks a loaded definition: its kind and name, and that its JSON parses back unchanged.
pub fn check_def(def: &ObjectDef, kind: ObjectKind, name: &str) -> Result<(), String> {
    if def.kind() != kind {
        return Err(format!("returned a {} definition", def.kind()));
    }
    if def.name() != name {
        return Err(format!("returned the definition of {:?}", def.name()));
    }
    let json = serde_json::to_value(def).map_err(|err| format!("does not serialize: {err}"))?;
    let back: ObjectDef = serde_json::from_value(json.clone())
        .map_err(|err| format!("its JSON does not parse back: {err}"))?;
    let again =
        serde_json::to_value(&back).map_err(|err| format!("does not serialize again: {err}"))?;
    if again != json {
        return Err("its JSON does not round-trip".to_owned());
    }
    Ok(())
}

/// Up to ten listed names, for error messages.
fn preview(rows: &[ObjectSummary]) -> String {
    const MAX: usize = 10;
    let mut names: Vec<&str> = rows.iter().take(MAX).map(|r| r.obj.name.as_str()).collect();
    if rows.len() > MAX {
        names.push("...");
    }
    if names.is_empty() {
        "nothing".to_owned()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use oxbase_driver::mock::MockDriver;
    use oxbase_driver::{ConnectParams, Driver as _};
    use oxbase_types::{Error, Extras, Result, SchemaDef, ViewDef};

    use super::*;

    /// Lists fixed rows and loads a view (or schema) definition of the requested ref.
    struct Fake {
        rows: Vec<ObjectSummary>,
        /// Overrides the loaded definition's name.
        load_name: Option<&'static str>,
        fail_list: bool,
    }

    impl Fake {
        fn new(rows: Vec<ObjectSummary>) -> Self {
            Self {
                rows,
                load_name: None,
                fail_list: false,
            }
        }
    }

    #[async_trait]
    impl Fetcher for Fake {
        async fn list(
            &self,
            _c: &dyn Connection,
            _parent: &ObjectRef,
            _cancel: CancellationToken,
        ) -> Result<Vec<ObjectSummary>> {
            if self.fail_list {
                return Err(Error::unsupported("boom"));
            }
            Ok(self.rows.clone())
        }

        async fn load(
            &self,
            _c: &dyn Connection,
            obj: &ObjectRef,
            _cancel: CancellationToken,
        ) -> Result<ObjectDef> {
            let mut obj = obj.clone();
            if let Some(n) = self.load_name {
                n.clone_into(&mut obj.name);
            }
            Ok(if obj.kind == ObjectKind::View {
                ObjectDef::View(ViewDef {
                    obj,
                    definition: "SELECT 1".to_owned(),
                    columns: Vec::new(),
                    comment: None,
                    extras: Extras::default(),
                })
            } else {
                ObjectDef::Schema(SchemaDef::new(obj))
            })
        }
    }

    async fn conn() -> Box<dyn Connection> {
        MockDriver::new(Engine::Postgres)
            .connect(&ConnectParams::new(Engine::Postgres))
            .await
            .unwrap()
    }

    fn db() -> ObjectRef {
        ObjectRef::database("oxbase_test")
    }

    fn views(names: &[&str]) -> Vec<ObjectSummary> {
        names
            .iter()
            .map(|n| ObjectSummary::new(db().child(ObjectKind::View, *n)))
            .collect()
    }

    fn group(names: &[&str]) -> Vec<SeedGroup> {
        vec![SeedGroup {
            parent: db(),
            names: names.iter().map(|&n| n.to_owned()).collect(),
        }]
    }

    #[tokio::test]
    async fn passes_when_every_seed_is_listed_and_loads() {
        let c = conn().await;
        let f = Fake::new(views(&["a_view", "m2_view"]));
        let detail = check_kind(&f, c.as_ref(), ObjectKind::View, &group(&["m2_view"]))
            .await
            .unwrap();
        assert_eq!(
            detail,
            "m2_view under database:oxbase_test: listed, 1 loaded (2 rows)"
        );
    }

    #[tokio::test]
    async fn databases_list_under_the_root() {
        let c = conn().await;
        let rows = vec![ObjectSummary::new(
            ObjectRef::root().child(ObjectKind::Database, "oxbase_test"),
        )];
        let groups = vec![SeedGroup {
            parent: ObjectRef::root(),
            names: vec!["oxbase_test".to_owned()],
        }];
        let detail = check_kind(&Fake::new(rows), c.as_ref(), ObjectKind::Database, &groups)
            .await
            .unwrap();
        assert!(
            detail.starts_with("oxbase_test under the server: listed"),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn fails_on_a_missing_seed() {
        let c = conn().await;
        let f = Fake::new(views(&["other"]));
        let err = check_kind(&f, c.as_ref(), ObjectKind::View, &group(&["m2_view"]))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            "m2_view not listed under database:oxbase_test (listed: other)"
        );
    }

    #[tokio::test]
    async fn fails_on_a_ref_outside_the_parent_convention() {
        let c = conn().await;
        // No catalog: the fetcher did not build the ref with `parent.child`.
        let f = Fake::new(vec![ObjectSummary::new(ObjectRef::new(
            ObjectKind::View,
            "m2_view",
        ))]);
        let err = check_kind(&f, c.as_ref(), ObjectKind::View, &group(&["m2_view"]))
            .await
            .unwrap_err();
        assert!(err.contains("expected"), "{err}");
    }

    #[tokio::test]
    async fn fails_on_system_rows_wrong_kinds_bad_loads_and_list_errors() {
        let c = conn().await;
        let mut rows = views(&["m2_view"]);
        rows[0].system = true;
        let err = check_kind(
            &Fake::new(rows),
            c.as_ref(),
            ObjectKind::View,
            &group(&["m2_view"]),
        )
        .await
        .unwrap_err();
        assert!(err.contains("system object"), "{err}");

        let rows = vec![ObjectSummary::new(db().child(ObjectKind::Table, "t"))];
        let err = check_kind(
            &Fake::new(rows),
            c.as_ref(),
            ObjectKind::View,
            &group(&["t"]),
        )
        .await
        .unwrap_err();
        assert!(err.contains("(a table)"), "{err}");

        let mut f = Fake::new(views(&["m2_view"]));
        f.load_name = Some("other");
        let err = check_kind(&f, c.as_ref(), ObjectKind::View, &group(&["m2_view"]))
            .await
            .unwrap_err();
        assert!(err.contains("definition of \"other\""), "{err}");

        let mut f = Fake::new(Vec::new());
        f.fail_list = true;
        let err = check_kind(&f, c.as_ref(), ObjectKind::View, &group(&["m2_view"]))
            .await
            .unwrap_err();
        assert!(
            err.starts_with("list under database:oxbase_test: "),
            "{err}"
        );
    }

    #[test]
    fn routine_signatures_and_implicit_table_parents_are_normalized() {
        let table = ObjectRef::table(Some("db"), Some("s"), "t");
        let mut col = table.child(ObjectKind::Column, "c");
        col.parent_kind = None;
        assert_eq!(normalized(&col), table.child(ObjectKind::Column, "c"));
        let f = ObjectRef::schema(Some("db"), "s")
            .child(ObjectKind::Function, "f")
            .with_signature("n integer");
        assert_eq!(f.signature.as_deref(), Some("n integer"));
        assert_eq!(normalized(&f).signature, None);
    }

    #[test]
    fn check_def_rejects_the_wrong_kind() {
        let def = ObjectDef::Schema(SchemaDef::new(ObjectRef::database("d")));
        assert!(check_def(&def, ObjectKind::Database, "d").is_ok());
        assert_eq!(
            check_def(&def, ObjectKind::Schema, "d").unwrap_err(),
            "returned a database definition"
        );
    }

    const NO_KINDS: Vec<ObjectKind> = Vec::new();

    #[test]
    fn engines_without_a_driver_or_fetchers_run_nothing() {
        let reg = adapter::provisional_registry();
        for engine in Engine::ALL {
            assert_eq!(runnable_kinds(&reg, engine), NO_KINDS, "{engine}");
        }
        let mut reg = adapter::provisional_registry();
        reg.register_fetcher(
            Engine::Redis,
            ObjectKind::Key,
            Arc::new(Fake::new(Vec::new())),
        );
        reg.register_fetcher(
            Engine::Sqlite,
            ObjectKind::View,
            Arc::new(Fake::new(Vec::new())),
        );
        // Redis has no driver before M7.
        assert_eq!(runnable_kinds(&reg, Engine::Redis), NO_KINDS);
        assert_eq!(runnable_kinds(&reg, Engine::Sqlite), [ObjectKind::View]);
    }

    #[test]
    fn quotes_fixture_identifiers() {
        assert_eq!(quote_ident("m2aux"), "\"m2aux\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }
}
