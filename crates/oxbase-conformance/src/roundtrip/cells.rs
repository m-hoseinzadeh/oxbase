//! One `roundtrip` cell and the `capabilities` cell that goes with it (see the module docs of
//! [`super`]).

use std::fmt::Write as _;
use std::future::Future;
use std::sync::Arc;

use oxbase_dialect::{
    Comparator, DdlContext, DdlGenerator, DesignerCapabilities, DialectRegistry, Fetcher, Quoting,
    Support,
};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_services::SessionHandle;
use oxbase_types::{
    CheckDef, ColumnDef, DataType, Engine, Error, Extras, FkAction, ForeignKeyDef, IndexColumn,
    IndexDef, ObjectDef, ObjectKind, ObjectRef, PrimaryKeyDef, PropId, PropertyDiff, Rename,
    Result, Script, ServerVersion, StepPhase, TableDef, Value,
};

use super::exec::{exec, run_script};
use crate::matrix::CellStatus;

/// Table comment set at create time (when the engine has table comments): a quote, a
/// backslash and non-ASCII text, so the generator's escaping is part of the round trip.
pub const TABLE_COMMENT: &str = "Round trip: it's a \\ test, caf\u{e9}";
/// Table comment after the alter.
pub const TABLE_COMMENT_CHANGED: &str = "Changed: \"quoted\" and it's \u{fc}";
/// Column comment (same characters as [`TABLE_COMMENT`]).
pub const COLUMN_COMMENT: &str = "Note: it's a \\ column, na\u{ef}ve";
/// Literal default of the added column, quoted with the engine's `Quoting::quote_str`.
pub const DEFAULT_TEXT: &str = "it's \u{fc}";
/// The column the column cell adds, then renames to [`RENAMED_COLUMN`].
pub const EXTRA_COLUMN: &str = "c_extra";
pub const RENAMED_COLUMN: &str = "c_renamed";
/// Check expressions before and after the alter (`qty` is a column of the base table).
pub const CHECK_EXPRESSION: &str = "qty >= 0";
pub const CHECK_EXPRESSION_CHANGED: &str = "qty >= 1";

/// What one cell runs with.
pub(crate) struct Env {
    pub engine: Engine,
    pub registry: Arc<DialectRegistry>,
    pub session: SessionHandle,
    pub version: ServerVersion,
    pub caps: DesignerCapabilities,
    pub quoting: Arc<dyn Quoting>,
    /// Where the scratch tables live (a schema or a database).
    pub container: ObjectRef,
    /// The cell's scratch tables ([`super::table_names`]).
    pub names: Vec<String>,
    pub cancel: CancellationToken,
}

/// Status and detail of the two cells.
pub(crate) struct CellReport {
    roundtrip: (CellStatus, String),
    capabilities: (CellStatus, String),
}

impl CellReport {
    /// Both cells failed for `why` (a panic or a timeout).
    pub(crate) fn failed(why: &str) -> Self {
        Self {
            roundtrip: (CellStatus::Fail, why.to_owned()),
            capabilities: (
                CellStatus::Pending,
                format!("not checked: the round trip did not finish ({why})"),
            ),
        }
    }

    pub(crate) fn into_parts(self) -> ((CellStatus, String), (CellStatus, String)) {
        (self.roundtrip, self.capabilities)
    }
}

/// Runs the round trip of `kind` (holding the session for the whole cell) and the capability
/// checks that go with it.
pub(crate) fn run(env: Arc<Env>, kind: ObjectKind) -> impl Future<Output = CellReport> + Send {
    async move {
        let _guard = env.session.exclusive().await;
        let roles = match Roles::resolve(&env.registry, env.engine, kind) {
            Ok(r) => r,
            Err(why) => {
                return CellReport {
                    roundtrip: (CellStatus::Pending, format!("pending ({why})")),
                    capabilities: (CellStatus::Pending, format!("pending ({why})")),
                };
            }
        };
        let conn = env.session.connection().clone();
        let mut run = Run {
            env: &env,
            roles,
            conn,
            agree: Vec::new(),
            disagree: Vec::new(),
            done: Vec::new(),
        };
        let result = match kind {
            ObjectKind::Table => run.table().await,
            ObjectKind::Column => run.column().await,
            ObjectKind::Index => run.index().await,
            ObjectKind::ForeignKey => run.foreign_key().await,
            ObjectKind::Check => run.check().await,
            other => Err(format!("no round trip is defined for {other}")),
        };
        run.report(result)
    }
}

/// The dialect roles one cell uses.
struct Roles {
    table_ddl: Arc<dyn DdlGenerator>,
    table_cmp: Arc<dyn Comparator>,
    tables: Arc<dyn Fetcher>,
    ddl: Arc<dyn DdlGenerator>,
    cmp: Arc<dyn Comparator>,
    fetch: Arc<dyn Fetcher>,
}

impl Roles {
    fn resolve(reg: &DialectRegistry, engine: Engine, kind: ObjectKind) -> Result<Self> {
        Ok(Self {
            table_ddl: reg.ddl(engine, ObjectKind::Table)?,
            table_cmp: reg.comparator(engine, ObjectKind::Table)?,
            tables: reg.fetcher(engine, ObjectKind::Table)?,
            ddl: reg.ddl(engine, kind)?,
            cmp: reg.comparator(engine, kind)?,
            fetch: reg.fetcher(engine, kind)?,
        })
    }
}

/// A generated script to run, or a change both the capabilities and the generator call
/// unsupported.
enum Gen {
    Run(Script),
    Skipped,
}

/// `Ok(None)`: the round trip passed. `Ok(Some(why))`: not applicable. `Err`: the first
/// problem.
type Outcome = std::result::Result<Option<String>, String>;
type Step<T> = std::result::Result<T, String>;

struct Run<'a> {
    env: &'a Env,
    roles: Roles,
    conn: Arc<dyn Connection>,
    /// Capability checks that agree, and those that do not.
    agree: Vec<String>,
    disagree: Vec<String>,
    /// The round-trip steps that passed, in order.
    done: Vec<String>,
}

/// One change of the column cell, applied to the fetched column.
type ColumnChange = fn(&mut ColumnDef, &dyn Quoting);

/// `Support` of a yes/no capability.
fn flag(on: bool) -> Support {
    if on {
        Support::Native
    } else {
        Support::Unsupported
    }
}

/// The weaker of two supports (a change that needs both a drop and an add).
fn both(a: Support, b: Support) -> Support {
    match (a, b) {
        (Support::Unsupported, _) | (_, Support::Unsupported) => Support::Unsupported,
        (Support::Rebuild, _) | (_, Support::Rebuild) => Support::Rebuild,
        _ => Support::Native,
    }
}

/// Up to eight differences, for messages.
fn list(diffs: &[PropertyDiff]) -> String {
    const MAX: usize = 8;
    let mut out = diffs
        .iter()
        .take(MAX)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    if diffs.len() > MAX {
        let _ = write!(out, "; ... ({} in all)", diffs.len());
    }
    out
}

/// `kind object .prop` of each difference, sorted: what changed, without the values (which
/// engines spell their own way).
fn change_keys(diffs: &[PropertyDiff]) -> Vec<String> {
    let mut keys: Vec<String> = diffs
        .iter()
        .map(|d| {
            format!(
                "{:?} {} .{}",
                d.kind,
                d.object,
                d.prop.as_ref().map_or("", PropId::as_str)
            )
        })
        .collect();
    keys.sort();
    keys
}

/// The child of `kind` named `name` in `table`, as a child definition.
pub(crate) fn child_of(table: &TableDef, kind: ObjectKind, name: &str) -> Option<ObjectDef> {
    let t = table.obj.clone();
    match kind {
        ObjectKind::Column => table.column(name).map(|c| ObjectDef::Column {
            table: t,
            column: c.clone(),
        }),
        ObjectKind::Index => {
            table
                .indexes
                .iter()
                .find(|i| i.name == name)
                .map(|i| ObjectDef::Index {
                    table: t,
                    index: i.clone(),
                })
        }
        ObjectKind::ForeignKey => table
            .foreign_keys
            .iter()
            .find(|f| f.name.as_deref() == Some(name))
            .map(|f| ObjectDef::ForeignKey {
                table: t,
                foreign_key: f.clone(),
            }),
        ObjectKind::Check => table
            .checks
            .iter()
            .find(|c| c.name.as_deref() == Some(name))
            .map(|c| ObjectDef::Check {
                table: t,
                check: c.clone(),
            }),
        _ => None,
    }
}

/// Replaces the item at `pos` with `new`, removes it (`new` is `None`) or appends `new`.
fn put<T>(items: &mut Vec<T>, pos: Option<usize>, new: Option<T>) {
    match (pos, new) {
        (Some(i), Some(n)) => items[i] = n,
        (Some(i), None) => {
            items.remove(i);
        }
        (None, Some(n)) => items.push(n),
        (None, None) => {}
    }
}

/// `table` with its child of `kind` named `old` replaced by `new` (added when `old` is `None`,
/// removed when `new` is `None`): the table a child change is meant to produce.
pub(crate) fn with_child(
    table: &TableDef,
    kind: ObjectKind,
    old: Option<&str>,
    new: Option<&ObjectDef>,
) -> TableDef {
    let mut t = table.clone();
    match kind {
        ObjectKind::Column => {
            let pos = old.and_then(|o| t.columns.iter().position(|c| c.name == o));
            let new = new.and_then(|d| match d {
                ObjectDef::Column { column, .. } => Some(column.clone()),
                _ => None,
            });
            put(&mut t.columns, pos, new);
        }
        ObjectKind::Index => {
            let pos = old.and_then(|o| t.indexes.iter().position(|i| i.name == o));
            let new = new.and_then(|d| match d {
                ObjectDef::Index { index, .. } => Some(index.clone()),
                _ => None,
            });
            put(&mut t.indexes, pos, new);
        }
        ObjectKind::ForeignKey => {
            let pos = old.and_then(|o| {
                t.foreign_keys
                    .iter()
                    .position(|f| f.name.as_deref() == Some(o))
            });
            let new = new.and_then(|d| match d {
                ObjectDef::ForeignKey { foreign_key, .. } => Some(foreign_key.clone()),
                _ => None,
            });
            put(&mut t.foreign_keys, pos, new);
        }
        ObjectKind::Check => {
            let pos = old.and_then(|o| t.checks.iter().position(|c| c.name.as_deref() == Some(o)));
            let new = new.and_then(|d| match d {
                ObjectDef::Check { check, .. } => Some(check.clone()),
                _ => None,
            });
            put(&mut t.checks, pos, new);
        }
        _ => {}
    }
    t
}

/// The base table every cell starts from: `id bigint NOT NULL` (primary key),
/// `code varchar(40) NOT NULL`, `qty bigint DEFAULT 0`, `note text`. Neutral types without a
/// native spelling, so each engine's generator picks its own.
pub(crate) fn base_table(table: ObjectRef) -> TableDef {
    let mut qty = ColumnDef::new("qty", DataType::INT64);
    qty.default = Some("0".to_owned());
    let mut t = TableDef::new(table);
    t.columns = vec![
        ColumnDef::new("id", DataType::INT64).not_null(),
        ColumnDef::new("code", DataType::VarChar { length: Some(40) }).not_null(),
        qty,
        ColumnDef::new("note", DataType::Text),
    ];
    t.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec!["id".to_owned()],
    });
    t
}

impl Run<'_> {
    fn ctx(&self) -> DdlContext<'_> {
        DdlContext::new(&self.env.version)
    }

    fn table_ref(&self, i: usize) -> ObjectRef {
        self.env
            .container
            .child(ObjectKind::Table, self.env.names[i].clone())
    }

    fn report(self, result: Outcome) -> CellReport {
        let done = if self.done.is_empty() {
            "nothing".to_owned()
        } else {
            self.done.join(", ")
        };
        let roundtrip = match &result {
            Ok(None) => (CellStatus::Pass, done),
            Ok(Some(why)) => (CellStatus::NotApplicable, why.clone()),
            Err(e) => (CellStatus::Fail, format!("{e} (passed before: {done})")),
        };
        let capabilities = if self.disagree.is_empty() && result.is_err() {
            (
                CellStatus::Pending,
                format!(
                    "not checked: the round trip failed ({} check(s) agreed so far)",
                    self.agree.len()
                ),
            )
        } else if self.disagree.is_empty() {
            (
                CellStatus::Pass,
                format!(
                    "{} check(s) agree: {}",
                    self.agree.len(),
                    self.agree.join("; ")
                ),
            )
        } else {
            (CellStatus::Fail, self.disagree.join("; "))
        };
        CellReport {
            roundtrip,
            capabilities,
        }
    }

    /// Records whether generating `op` agrees with the capabilities, and whether to run it.
    fn generated(&mut self, op: &str, support: Support, made: Result<Script>) -> Step<Gen> {
        match made {
            Ok(script) => {
                if support.is_supported() {
                    self.agree.push(format!("{op}: {support:?}"));
                } else {
                    self.disagree.push(format!(
                        "{op}: the designer capabilities say unsupported, but the generator \
                         produced a script"
                    ));
                }
                if support == Support::Rebuild && !script.is_empty() && !script.is_destructive() {
                    self.disagree.push(format!(
                        "{op}: the capabilities say it rebuilds the table, but no step is \
                         destructive"
                    ));
                }
                self.check_transactional(op, &script);
                Ok(Gen::Run(script))
            }
            Err(Error::Unsupported { message }) if !support.is_supported() => {
                self.agree
                    .push(format!("{op}: unsupported in both ({message})"));
                Ok(Gen::Skipped)
            }
            Err(e @ Error::Unsupported { .. }) => Err(format!(
                "{op}: the generator says {e}, but the designer capabilities say {support:?}"
            )),
            Err(e) => Err(format!("{op}: generating the script failed: {e}")),
        }
    }

    /// `transactionalDdl` against the script's steps: all main steps transactional when DDL is
    /// transactional, none when every statement commits on its own.
    fn check_transactional(&mut self, op: &str, script: &Script) {
        let main: Vec<bool> = script
            .steps
            .iter()
            .filter(|s| s.phase == StepPhase::Main)
            .map(|s| s.transactional)
            .collect();
        if main.is_empty() {
            return;
        }
        if self.env.caps.transactional_ddl && main.iter().any(|t| !t) {
            self.disagree.push(format!(
                "{op}: the capabilities say DDL is transactional, but a main step is marked \
                 non-transactional"
            ));
        } else if !self.env.caps.transactional_ddl && main.iter().any(|t| *t) {
            self.disagree.push(format!(
                "{op}: the capabilities say DDL commits implicitly, but a main step is marked \
                 transactional"
            ));
        }
    }

    async fn run_ddl(&self, op: &str, script: &Script) -> Step<()> {
        let atomic = self.env.caps.transactional_ddl && script.is_atomic();
        run_script(self.conn.as_ref(), script, atomic, &self.env.cancel)
            .await
            .map_err(|e| format!("{op}: running the script failed: {e}\n--- script ---\n{script}"))
    }

    async fn load(&self, fetcher: &dyn Fetcher, r: &ObjectRef, op: &str) -> Step<ObjectDef> {
        fetcher
            .load(self.conn.as_ref(), r, self.env.cancel.clone())
            .await
            .map_err(|e| format!("{op}: fetching {r} failed: {e}"))
    }

    async fn load_table(&self, r: &ObjectRef, op: &str) -> Step<TableDef> {
        match self.load(self.roles.tables.as_ref(), r, op).await? {
            ObjectDef::Table(t) => Ok(t),
            other => Err(format!(
                "{op}: fetching {r} returned a {} definition",
                other.kind()
            )),
        }
    }

    async fn expect_gone(&self, fetcher: &dyn Fetcher, r: &ObjectRef, op: &str) -> Step<()> {
        match fetcher
            .load(self.conn.as_ref(), r, self.env.cancel.clone())
            .await
        {
            Err(Error::NotFound { .. }) => Ok(()),
            Ok(_) => Err(format!("{op}: {r} can still be fetched")),
            Err(e) => Err(format!("{op}: fetching {r} should say not found, got: {e}")),
        }
    }

    async fn listed(&self, name: &str, op: &str) -> Step<bool> {
        let rows = self
            .roles
            .tables
            .list(
                self.conn.as_ref(),
                &self.env.container,
                self.env.cancel.clone(),
            )
            .await
            .map_err(|e| format!("{op}: listing {} failed: {e}", self.env.container))?;
        Ok(rows.iter().any(|r| r.obj.name == name))
    }

    /// The comparator finds no difference between what the script was generated from and what
    /// the server has now.
    fn same(cmp: &dyn Comparator, intended: &ObjectDef, fetched: &ObjectDef, op: &str) -> Step<()> {
        let diffs = cmp.diff(intended, fetched);
        if diffs.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{op}: the fetched {} differs from the definition the script was generated \
                 from: {}",
                intended.kind(),
                list(&diffs)
            ))
        }
    }

    /// The table changed exactly as intended: `before → after` reports the same objects,
    /// properties and kinds of change as `before → intended`.
    fn change_only(
        &self,
        before: &TableDef,
        after: &TableDef,
        intended: &TableDef,
        op: &str,
    ) -> Step<()> {
        let cmp = self.roles.table_cmp.as_ref();
        let (b, a, i) = (
            ObjectDef::Table(before.clone()),
            ObjectDef::Table(after.clone()),
            ObjectDef::Table(intended.clone()),
        );
        let seen = cmp.diff(&b, &a);
        let meant = cmp.diff(&b, &i);
        if change_keys(&seen) == change_keys(&meant) {
            Ok(())
        } else {
            Err(format!(
                "{op}: the table changed by [{}], but the intended change is [{}]",
                list(&seen),
                list(&meant)
            ))
        }
    }

    /// Creates `def` with the `Table` generator and checks the round trip. `None` when both
    /// the capabilities and the generator say tables cannot be created.
    async fn create_table(&mut self, def: &TableDef, op: &str) -> Step<Option<TableDef>> {
        let made = self
            .roles
            .table_ddl
            .create(&ObjectDef::Table(def.clone()), &self.ctx());
        let Gen::Run(script) = self.generated(op, flag(self.env.caps.create_table), made)? else {
            return Ok(None);
        };
        self.run_ddl(op, &script).await?;
        let fetched = self.load_table(&def.obj, op).await?;
        Self::same(
            self.roles.table_cmp.as_ref(),
            &ObjectDef::Table(def.clone()),
            &ObjectDef::Table(fetched.clone()),
            op,
        )?;
        if !self.listed(&def.obj.name, op).await? {
            return Err(format!("{op}: {} is not listed after create", def.obj));
        }
        self.done.push(op.to_owned());
        Ok(Some(fetched))
    }

    /// A base table for a child cell (must work).
    async fn base(&mut self, def: &TableDef) -> Step<TableDef> {
        let op = format!("create base table {}", def.obj.name);
        self.create_table(def, &op)
            .await?
            .ok_or_else(|| format!("{op}: tables cannot be created on this engine"))
    }

    /// Alters table `cur` into `new` with the `Table` generator and checks the round trip.
    async fn alter_table(
        &mut self,
        op: &str,
        support: Support,
        cur: &TableDef,
        new: &TableDef,
        renames: &[Rename],
    ) -> Step<Option<TableDef>> {
        let ctx = self.ctx().with_table(cur).with_renames(renames);
        let made = self.roles.table_ddl.alter(
            &ObjectDef::Table(cur.clone()),
            &ObjectDef::Table(new.clone()),
            &ctx,
        );
        let Gen::Run(script) = self.generated(op, support, made)? else {
            return Ok(None);
        };
        if script.is_empty() {
            return Err(format!("{op}: the generator produced an empty script"));
        }
        self.run_ddl(op, &script).await?;
        let after = self.load_table(&new.obj, op).await?;
        Self::same(
            self.roles.table_cmp.as_ref(),
            &ObjectDef::Table(new.clone()),
            &ObjectDef::Table(after.clone()),
            op,
        )?;
        self.change_only(cur, &after, new, op)?;
        if cur.obj.name != new.obj.name {
            self.expect_gone(self.roles.tables.as_ref(), &cur.obj, op)
                .await?;
        }
        self.done.push(op.to_owned());
        Ok(Some(after))
    }

    /// Adds (`old` is `None`), changes or drops (`new` is `None`) a child of `table` with the
    /// kind's generator, then checks the child and the whole table. Returns the table as fetched
    /// afterwards, or `None` when the change was skipped (unsupported in both).
    async fn child_step(
        &mut self,
        op: &str,
        support: Support,
        table: &TableDef,
        old: Option<&ObjectDef>,
        new: Option<&ObjectDef>,
        renames: &[Rename],
    ) -> Step<Option<TableDef>> {
        let ctx = self.ctx().with_table(table).with_renames(renames);
        let ddl = self.roles.ddl.clone();
        let made = match (old, new) {
            (None, Some(n)) => ddl.create(n, &ctx),
            (Some(o), Some(n)) => ddl.alter(o, n, &ctx),
            (Some(o), None) => ddl.drop_object(&o.object_ref(), &ctx),
            (None, None) => return Ok(None),
        };
        let Gen::Run(script) = self.generated(op, support, made)? else {
            return Ok(None);
        };
        if script.is_empty() {
            return Err(format!("{op}: the generator produced an empty script"));
        }
        self.run_ddl(op, &script).await?;
        if let Some(n) = new {
            let fetched = self
                .load(self.roles.fetch.as_ref(), &n.object_ref(), op)
                .await?;
            Self::same(self.roles.cmp.as_ref(), n, &fetched, op)?;
        }
        if let Some(o) = old
            && new.is_none_or(|n| n.name() != o.name())
        {
            self.expect_gone(self.roles.fetch.as_ref(), &o.object_ref(), op)
                .await?;
        }
        let kind = new.or(old).map_or(ObjectKind::Table, ObjectDef::kind);
        let intended = with_child(table, kind, old.map(ObjectDef::name), new);
        let after = self.load_table(&table.obj, op).await?;
        Self::same(
            self.roles.table_cmp.as_ref(),
            &ObjectDef::Table(intended.clone()),
            &ObjectDef::Table(after.clone()),
            &format!("{op} (whole table)"),
        )?;
        self.change_only(table, &after, &intended, op)?;
        self.done.push(op.to_owned());
        Ok(Some(after))
    }

    // --- the cells ---------------------------------------------------------------------------

    /// create (with a comment where supported) → rename → change the comment → drop; and the
    /// transactional-DDL probe.
    async fn table(&mut self) -> Outcome {
        let mut def = base_table(self.table_ref(0));
        if self.env.caps.table_comment.is_supported() {
            def.comment = Some(TABLE_COMMENT.to_owned());
        }
        let Some(mut cur) = self.create_table(&def, "create table").await? else {
            return Ok(Some(
                "the designer capabilities and the generator both say tables cannot be created"
                    .to_owned(),
            ));
        };

        let mut renamed = cur.clone();
        renamed.obj = self.table_ref(1);
        let renames = [Rename::new(
            ObjectKind::Table,
            cur.obj.name.clone(),
            renamed.obj.name.clone(),
        )];
        if let Some(after) = self
            .alter_table(
                "rename table",
                self.env.caps.rename_table,
                &cur,
                &renamed,
                &renames,
            )
            .await?
        {
            cur = after;
        }

        let mut commented = cur.clone();
        commented.comment = Some(TABLE_COMMENT_CHANGED.to_owned());
        if let Some(after) = self
            .alter_table(
                "change table comment",
                self.env.caps.table_comment,
                &cur,
                &commented,
                &[],
            )
            .await?
        {
            cur = after;
        }

        let op = "drop table";
        let made = self
            .roles
            .table_ddl
            .drop_object(&cur.obj, &self.ctx().with_table(&cur));
        if let Gen::Run(script) = self.generated(op, flag(self.env.caps.drop_table), made)? {
            self.run_ddl(op, &script).await?;
            self.expect_gone(self.roles.tables.as_ref(), &cur.obj, op)
                .await?;
            if self.listed(&cur.obj.name, op).await? {
                return Err(format!("{op}: {} is still listed", cur.obj));
            }
            self.done.push(op.to_owned());
        }

        self.probe_transactional().await;
        Ok(None)
    }

    /// `transactionalDdl` against the server: a `CREATE TABLE` run in a transaction that is
    /// rolled back is gone exactly when DDL is transactional. A probe that cannot run is a
    /// disagreement, not a round-trip failure.
    async fn probe_transactional(&mut self) {
        let def = base_table(self.table_ref(2));
        match self.rolled_back_create(&def).await {
            Ok(rolled_back) if rolled_back == self.env.caps.transactional_ddl => {
                self.agree
                    .push(format!("transactionalDdl = {rolled_back} (probed)"));
            }
            Ok(rolled_back) => self.disagree.push(format!(
                "the capabilities say transactionalDdl = {}, but a CREATE TABLE in a rolled-back \
                 transaction {}",
                self.env.caps.transactional_ddl,
                if rolled_back { "was undone" } else { "stayed" }
            )),
            Err(e) => self.disagree.push(format!("transactional DDL probe: {e}")),
        }
    }

    /// Runs the `CREATE TABLE` of `def` in a transaction, rolls it back and says whether the
    /// table is gone.
    async fn rolled_back_create(&self, def: &TableDef) -> Step<bool> {
        let op = "transactional DDL probe";
        let script = self
            .roles
            .table_ddl
            .create(&ObjectDef::Table(def.clone()), &self.ctx())
            .map_err(|e| format!("cannot generate CREATE TABLE: {e}"))?;
        let tx = self
            .conn
            .begin()
            .await
            .map_err(|e| format!("cannot begin a transaction: {e}"))?;
        for step in script.steps.iter().filter(|s| s.phase == StepPhase::Main) {
            let stream = tx
                .execute(&step.sql, &[], self.env.cancel.clone())
                .await
                .map_err(|e| format!("{}: {e}", step.sql))?;
            oxbase_driver::collect(stream)
                .await
                .map_err(|e| format!("{}: {e}", step.sql))?;
        }
        tx.rollback()
            .await
            .map_err(|e| format!("rollback failed: {e}"))?;
        Ok(!self.listed(&def.obj.name, op).await?)
    }

    /// add a column → type → NOT NULL → literal default → comment → rename → drop.
    async fn column(&mut self) -> Outcome {
        let mut cur = self.base(&base_table(self.table_ref(0))).await?;
        let caps = self.env.caps.clone();
        let table = cur.obj.clone();
        let added = ObjectDef::Column {
            table: table.clone(),
            column: ColumnDef::new(EXTRA_COLUMN, DataType::VarChar { length: Some(40) }),
        };
        let Some(after) = self
            .child_step("add column", caps.add_column, &cur, None, Some(&added), &[])
            .await?
        else {
            return Ok(Some(
                "the designer capabilities and the generator both say columns cannot be added"
                    .to_owned(),
            ));
        };
        cur = after;
        let mut name = EXTRA_COLUMN.to_owned();

        let changes: [(&str, Support, ColumnChange); 4] = [
            ("change column type", caps.alter_column_type, |c, _| {
                c.data_type = DataType::VarChar { length: Some(80) };
                c.native_type = String::new();
            }),
            (
                "set column NOT NULL",
                caps.alter_column_nullability,
                |c, _| {
                    c.nullable = false;
                },
            ),
            ("set column default", caps.alter_column_default, |c, q| {
                c.default = Some(q.quote_str(DEFAULT_TEXT));
            }),
            ("set column comment", caps.column_comment, |c, _| {
                c.comment = Some(COLUMN_COMMENT.to_owned());
            }),
        ];
        for (op, support, change) in changes {
            let old = child_of(&cur, ObjectKind::Column, &name)
                .ok_or_else(|| format!("{op}: column {name} is not in the fetched table"))?;
            let mut new = old.clone();
            if let ObjectDef::Column { column, .. } = &mut new {
                change(column, self.env.quoting.as_ref());
            }
            if let Some(after) = self
                .child_step(op, support, &cur, Some(&old), Some(&new), &[])
                .await?
            {
                cur = after;
            }
        }

        let op = "rename column";
        let old = child_of(&cur, ObjectKind::Column, &name)
            .ok_or_else(|| format!("{op}: column {name} is not in the fetched table"))?;
        let mut new = old.clone();
        if let ObjectDef::Column { column, .. } = &mut new {
            RENAMED_COLUMN.clone_into(&mut column.name);
        }
        let renames = [Rename::new(
            ObjectKind::Column,
            name.clone(),
            RENAMED_COLUMN,
        )];
        if let Some(after) = self
            .child_step(
                op,
                caps.rename_column,
                &cur,
                Some(&old),
                Some(&new),
                &renames,
            )
            .await?
        {
            cur = after;
            RENAMED_COLUMN.clone_into(&mut name);
        }

        let op = "drop column";
        let old = child_of(&cur, ObjectKind::Column, &name)
            .ok_or_else(|| format!("{op}: column {name} is not in the fetched table"))?;
        self.child_step(op, caps.drop_column, &cur, Some(&old), None, &[])
            .await?;
        Ok(None)
    }

    /// add an index on `code` → make it unique on `(code, qty)` → drop.
    async fn index(&mut self) -> Outcome {
        let mut cur = self.base(&base_table(self.table_ref(0))).await?;
        let caps = self.env.caps.clone();
        let name = format!("{}_ix", self.env.names[0]);
        let added = ObjectDef::Index {
            table: cur.obj.clone(),
            index: IndexDef {
                name: name.clone(),
                columns: vec![IndexColumn::column("code")],
                unique: false,
                is_constraint: false,
                predicate: None,
                extras: Extras::default(),
            },
        };
        let Some(after) = self
            .child_step("add index", caps.add_index, &cur, None, Some(&added), &[])
            .await?
        else {
            return Ok(Some(
                "the designer capabilities and the generator both say indexes cannot be added"
                    .to_owned(),
            ));
        };
        cur = after;

        let op = "change index";
        let old = child_of(&cur, ObjectKind::Index, &name)
            .ok_or_else(|| format!("{op}: index {name} is not in the fetched table"))?;
        let mut new = old.clone();
        if let ObjectDef::Index { index, .. } = &mut new {
            index.columns = vec![IndexColumn::column("code"), IndexColumn::column("qty")];
            index.unique = true;
        }
        if let Some(after) = self
            .child_step(
                op,
                both(caps.add_index, caps.drop_index),
                &cur,
                Some(&old),
                Some(&new),
                &[],
            )
            .await?
        {
            cur = after;
        }

        let op = "drop index";
        let old = child_of(&cur, ObjectKind::Index, &name)
            .ok_or_else(|| format!("{op}: index {name} is not in the fetched table"))?;
        self.child_step(op, caps.drop_index, &cur, Some(&old), None, &[])
            .await?;
        Ok(None)
    }

    /// Notes whether `action` is one of the capabilities' `fkActions`.
    fn note_action(&mut self, action: FkAction) {
        if self.env.caps.fk_actions.contains(&action) {
            self.agree.push(format!("{action:?} is in fkActions"));
        } else {
            self.disagree.push(format!(
                "the round trip uses {action:?}, which is not in the capabilities' fkActions {:?}",
                self.env.caps.fk_actions
            ));
        }
    }

    /// A referenced table and a referencing one (with an index on the key column); add a
    /// foreign key (`ON DELETE CASCADE` where offered) → change its delete action → drop.
    async fn foreign_key(&mut self) -> Outcome {
        let caps = self.env.caps.clone();
        let mut parent = TableDef::new(self.table_ref(1));
        parent.columns = vec![ColumnDef::new("id", DataType::INT64).not_null()];
        parent.primary_key = Some(PrimaryKeyDef {
            name: None,
            columns: vec!["id".to_owned()],
        });
        let parent = self.base(&parent).await?;

        let mut child = base_table(self.table_ref(0));
        child
            .columns
            .push(ColumnDef::new("ref_id", DataType::INT64));
        // An explicit index on the key column, so engines that need one for a foreign key
        // (InnoDB) use it instead of adding their own.
        child.indexes.push(IndexDef {
            name: format!("{}_ix", self.env.names[0]),
            columns: vec![IndexColumn::column("ref_id")],
            unique: false,
            is_constraint: false,
            predicate: None,
            extras: Extras::default(),
        });
        let mut cur = self.base(&child).await?;

        let name = format!("{}_fk", self.env.names[0]);
        let pick = |wanted: &[FkAction], not: FkAction| -> FkAction {
            wanted
                .iter()
                .copied()
                .find(|a| *a != not && caps.fk_actions.contains(a))
                .unwrap_or(wanted[0])
        };
        let on_delete = pick(
            &[FkAction::Cascade, FkAction::NoAction],
            FkAction::SetDefault,
        );
        let fk = ForeignKeyDef {
            name: Some(name.clone()),
            columns: vec!["ref_id".to_owned()],
            ref_table: parent.obj.clone(),
            ref_columns: vec!["id".to_owned()],
            on_delete,
            on_update: FkAction::NoAction,
            extras: Extras::default(),
        };
        self.note_action(on_delete);
        self.note_action(FkAction::NoAction);
        let added = ObjectDef::ForeignKey {
            table: cur.obj.clone(),
            foreign_key: fk,
        };
        let Some(after) = self
            .child_step(
                "add foreign key",
                caps.add_foreign_key,
                &cur,
                None,
                Some(&added),
                &[],
            )
            .await?
        else {
            return Ok(Some(
                "the designer capabilities and the generator both say foreign keys cannot be \
                 added"
                    .to_owned(),
            ));
        };
        cur = after;

        let op = "change foreign key";
        let old = child_of(&cur, ObjectKind::ForeignKey, &name)
            .ok_or_else(|| format!("{op}: foreign key {name} is not in the fetched table"))?;
        let next = pick(
            &[FkAction::SetNull, FkAction::Restrict, FkAction::NoAction],
            on_delete,
        );
        self.note_action(next);
        let mut new = old.clone();
        if let ObjectDef::ForeignKey { foreign_key, .. } = &mut new {
            foreign_key.on_delete = next;
        }
        if let Some(after) = self
            .child_step(
                op,
                both(caps.add_foreign_key, caps.drop_foreign_key),
                &cur,
                Some(&old),
                Some(&new),
                &[],
            )
            .await?
        {
            cur = after;
        }

        let op = "drop foreign key";
        let old = child_of(&cur, ObjectKind::ForeignKey, &name)
            .ok_or_else(|| format!("{op}: foreign key {name} is not in the fetched table"))?;
        self.child_step(op, caps.drop_foreign_key, &cur, Some(&old), None, &[])
            .await?;
        Ok(None)
    }

    /// add a check → probe enforcement → change its expression → drop.
    async fn check(&mut self) -> Outcome {
        let mut cur = self.base(&base_table(self.table_ref(0))).await?;
        let caps = self.env.caps.clone();
        let name = format!("{}_ck", self.env.names[0]);
        let added = ObjectDef::Check {
            table: cur.obj.clone(),
            check: CheckDef {
                name: Some(name.clone()),
                expression: CHECK_EXPRESSION.to_owned(),
                extras: Extras::default(),
            },
        };
        let Some(after) = self
            .child_step("add check", caps.add_check, &cur, None, Some(&added), &[])
            .await?
        else {
            return Ok(Some(
                "the designer capabilities and the generator both say checks cannot be added"
                    .to_owned(),
            ));
        };
        cur = after;
        self.probe_enforced(&cur.obj).await?;

        let op = "change check";
        let old = child_of(&cur, ObjectKind::Check, &name)
            .ok_or_else(|| format!("{op}: check {name} is not in the fetched table"))?;
        let mut new = old.clone();
        if let ObjectDef::Check { check, .. } = &mut new {
            CHECK_EXPRESSION_CHANGED.clone_into(&mut check.expression);
        }
        if let Some(after) = self
            .child_step(
                op,
                both(caps.add_check, caps.drop_check),
                &cur,
                Some(&old),
                Some(&new),
                &[],
            )
            .await?
        {
            cur = after;
        }

        let op = "drop check";
        let old = child_of(&cur, ObjectKind::Check, &name)
            .ok_or_else(|| format!("{op}: check {name} is not in the fetched table"))?;
        self.child_step(op, caps.drop_check, &cur, Some(&old), None, &[])
            .await?;
        Ok(None)
    }

    /// `checksEnforced` against the server: insert a row that violates the check (values bound,
    /// identifiers quoted); it must fail exactly when checks are enforced. The row is deleted
    /// again when it got in.
    async fn probe_enforced(&mut self, table: &ObjectRef) -> Step<()> {
        let q = self.env.quoting.as_ref();
        let sql = format!(
            "INSERT INTO {} ({}, {}, {}) VALUES ({}, {}, {})",
            q.qualify(table),
            q.quote_ident("id"),
            q.quote_ident("code"),
            q.quote_ident("qty"),
            q.placeholder(1),
            q.placeholder(2),
            q.placeholder(3)
        );
        let params = [
            Value::Int(1),
            Value::Text("probe".to_owned()),
            Value::Int(-1),
        ];
        let inserted = exec(self.conn.as_ref(), &sql, &params, &self.env.cancel)
            .await
            .is_ok();
        if inserted {
            let delete = format!("DELETE FROM {}", q.qualify(table));
            exec(self.conn.as_ref(), &delete, &[], &self.env.cancel)
                .await
                .map_err(|e| format!("check enforcement probe: {delete}: {e}"))?;
        }
        let enforced = !inserted;
        if enforced == self.env.caps.checks_enforced {
            self.agree
                .push(format!("checksEnforced = {enforced} (probed)"));
        } else {
            self.disagree.push(format!(
                "the capabilities say checksEnforced = {}, but a row violating the check was {}",
                self.env.caps.checks_enforced,
                if inserted { "accepted" } else { "rejected" }
            ));
        }
        Ok(())
    }
}
