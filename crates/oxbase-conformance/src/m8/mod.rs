//! M8 conformance (`docs/m8-contracts.md` §12, *conformance*): structure sync, data sync, the
//! transfer matrix and backup / restore, live against the Docker fixtures of the five SQL
//! engines (PostgreSQL, MySQL, MariaDB, SQL Server, SQLite).
//!
//! # Cells
//!
//! Per engine:
//!
//! - `seed`: fresh scratch copies of the `[m8]` sync pair (source and target), an empty restore
//!   container and an empty transfer target are built from the engine's seed script
//!   ([`seed`]); the copy holds the designed objects and row counts, and the canonical seed of
//!   the Docker init (`m8_src`, ...) exists. Every other cell runs on these copies, so a re-run
//!   (or another stream's live test that synced `m8_tgt`) cannot change what it sees.
//! - `sync_structure`: compare source / target with drops on (`StructureSyncService`); every
//!   `[m8] structure_items` id (minus the engine's `skip_items`) is reported as differing; apply
//!   every item (destructive confirmed); the script commits, the apply's re-compare is in sync,
//!   and a fresh compare reports zero differences.
//! - `sync_data`: compare the `[m8] data_tables` (`DataSyncService`); counts equal
//!   `[m8] data_expected`, the key-less table is refused; apply inserts, updates and deletes
//!   (confirmed); the apply reports in sync and a fresh compare has no differences.
//! - `backup`: back up the source container (`BackupService`), list and verify the archive,
//!   restore it into the empty restore container; every object restores, every table holds the
//!   source's row count, a structure compare source / restore is in sync and a data compare of
//!   the keyed data tables is in sync.
//!
//! The transfer matrix (`transfer`, one cell per ordered pair of different engines): copy the
//! source copy's `m8_data` into the target engine's transfer container (`TransferService`):
//! the plan must be runnable (or name a precise reason per blocked column), the run must
//! finish, and the target table must hold the same ids, names and notes. Non-exact column
//! mappings are listed with their reasons (the lossiness). PostgreSQL → MySQL is the M8 exit
//! check and must pass: it also copies the `[m8.transfer]` set (tables listed child first),
//! whose plan fidelities and `information_schema.COLUMNS.COLUMN_TYPE` must equal
//! `[m8.transfer.expected_mysql]`, with the rows and the foreign keys.
//!
//! # Pending
//!
//! Until a stream lands, its bodies are `unimplemented!("m8: <stream>")`. A cell whose roles are
//! not registered (transfer: a `TypeMapper` on both engines; structure sync: the sync-kind
//! generators and comparators of `DialectRegistry::missing_m8_roles`) is pending without
//! running; a cell that reaches an unfinished body is pending too ([`skeleton`]), naming the
//! stream. With [`REQUIRE_ENV`] (`OXBASE_CONFORMANCE_REQUIRE_M8=1`, set by the integrator once
//! every stream is merged) every cell must pass and `missing_m8_roles` must be empty: pending
//! or skipped cells fail the gate ([`M8Report::problems`]).
//!
//! | Variable | Effect |
//! |---|---|
//! | `OXBASE_CONFORMANCE_REQUIRE_M8=1` | The M8 guard (above) |
//! | `OXBASE_CONFORMANCE_SKIP_M8=1` | Report every cell as skipped (not with the guard) |
//! | `OXBASE_CONFORMANCE_KEEP_M8_SCRATCH=1` | Keep the scratch containers for inspection (a later run drops them once stale) |
//! | `OXBASE_CONFORMANCE_ALLOW_MISSING=1` | An unreachable fixture makes its cells skipped instead of failed |

pub mod seed;
pub mod skeleton;

mod live;

use std::fmt::Write as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use oxbase_dialect::{DialectRegistry, M8_ENGINES};
use serde::{Deserialize, Serialize};

use crate::adapter::{self, Engine};
use crate::fixtures::{Fixtures, FixturesError, ResolvedEngine};
use crate::matrix::CellStatus;
use crate::report::Summary;
use crate::runner::is_truthy;

/// Turns the M8 guard on (module docs).
pub const REQUIRE_ENV: &str = "OXBASE_CONFORMANCE_REQUIRE_M8";
/// Skips every M8 cell.
pub const SKIP_ENV: &str = "OXBASE_CONFORMANCE_SKIP_M8";
/// Keeps the scratch containers.
pub const KEEP_ENV: &str = "OXBASE_CONFORMANCE_KEEP_M8_SCRATCH";
/// `<target>/conformance/<REPORT_FILE>`.
pub const REPORT_FILE: &str = "m8-report.json";
/// Bumped when the JSON layout changes incompatibly.
pub const M8_REPORT_SCHEMA_VERSION: u32 = 1;

/// One kind of M8 cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum M8Check {
    Seed,
    SyncStructure,
    SyncData,
    Backup,
    Transfer,
}

impl M8Check {
    /// The per-engine cells, in run and column order.
    pub const PER_ENGINE: [Self; 4] = [
        Self::Seed,
        Self::SyncStructure,
        Self::SyncData,
        Self::Backup,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::SyncStructure => "sync_structure",
            Self::SyncData => "sync_data",
            Self::Backup => "backup",
            Self::Transfer => "transfer",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Seed => {
                "scratch copies of the [m8] pair built from the seed script; canonical seed present"
            }
            Self::SyncStructure => {
                "structure compare, apply every item (drops confirmed), re-compare in sync"
            }
            Self::SyncData => {
                "data compare counts as [m8] data_expected, apply with deletes, re-compare in sync"
            }
            Self::Backup => {
                "backup, list, verify, restore into an empty container, compare in sync"
            }
            Self::Transfer => {
                "m8_data (and PG -> MySQL: the [m8.transfer] set) copied and read back"
            }
        }
    }
}

impl std::fmt::Display for M8Check {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One M8 cell: per engine, or (transfer) per ordered engine pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct M8Cell {
    /// The fixture name (the transfer source for `transfer`).
    pub engine: String,
    pub engine_kind: Engine,
    pub check: M8Check,
    /// `transfer`: the target fixture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_kind: Option<Engine>,
    pub status: CellStatus,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

impl M8Cell {
    fn new(
        e: &ResolvedEngine,
        check: M8Check,
        status: CellStatus,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            engine: e.name.clone(),
            engine_kind: e.engine,
            check,
            target: None,
            target_kind: None,
            status,
            detail: detail.into(),
            elapsed_ms: None,
        }
    }

    fn transfer(
        source: &ResolvedEngine,
        target: &ResolvedEngine,
        status: CellStatus,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            target: Some(target.name.clone()),
            target_kind: Some(target.engine),
            ..Self::new(source, M8Check::Transfer, status, detail)
        }
    }

    /// `engine check` or `source -> target transfer`, for messages.
    pub fn label(&self) -> String {
        match &self.target {
            Some(t) => format!("{} -> {t} {}", self.engine, self.check),
            None => format!("{} {}", self.engine, self.check),
        }
    }
}

/// How the M8 run behaves (module docs, environment table).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct M8Options {
    pub require: bool,
    pub skip: bool,
    pub keep_scratch: bool,
    pub allow_missing: bool,
}

impl M8Options {
    /// Reads [`REQUIRE_ENV`], [`SKIP_ENV`], [`KEEP_ENV`] and `OXBASE_CONFORMANCE_ALLOW_MISSING`.
    pub fn from_env() -> Self {
        let var = |name: &str| is_truthy(std::env::var(name).ok().as_deref());
        Self {
            require: var(REQUIRE_ENV),
            skip: var(SKIP_ENV),
            keep_scratch: var(KEEP_ENV),
            allow_missing: var("OXBASE_CONFORMANCE_ALLOW_MISSING"),
        }
    }
}

/// The M8 run's result: a table per engine and the transfer matrix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct M8Report {
    pub report_schema_version: u32,
    pub generated_at_unix: u64,
    pub require_m8: bool,
    /// The M8 fixtures, in run order.
    pub engines: Vec<String>,
    pub cells: Vec<M8Cell>,
    pub summary: Summary,
    /// `DialectRegistry::missing_m8_roles` of every M8 engine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_roles: Vec<String>,
}

impl M8Report {
    fn new(
        engines: &[&ResolvedEngine],
        cells: Vec<M8Cell>,
        missing_roles: Vec<String>,
        require: bool,
    ) -> Self {
        Self {
            report_schema_version: M8_REPORT_SCHEMA_VERSION,
            generated_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            require_m8: require,
            engines: engines.iter().map(|e| e.name.clone()).collect(),
            summary: Summary::from_statuses(cells.iter().map(|c| c.status)),
            cells,
            missing_roles,
        }
    }

    /// The cell `check` of `engine` (transfer: from `engine` to `target`).
    pub fn cell(&self, engine: &str, check: M8Check, target: Option<&str>) -> Option<&M8Cell> {
        self.cells
            .iter()
            .find(|c| c.engine == engine && c.check == check && c.target.as_deref() == target)
    }

    /// What blocks the run: failed cells always; with `require` also pending cells, skipped
    /// cells (unless `allow_missing`), missing M8 roles and M8 engines without a fixture.
    pub fn problems(&self, opts: M8Options) -> Vec<String> {
        let mut out = Vec::new();
        for c in &self.cells {
            let bad = match c.status {
                CellStatus::Fail => true,
                CellStatus::Pending => opts.require,
                CellStatus::Skipped => opts.require && !opts.allow_missing,
                CellStatus::Pass | CellStatus::NotApplicable => false,
            };
            if bad {
                out.push(format!("{} {}: {}", c.label(), c.status.label(), c.detail));
            }
        }
        if opts.require {
            out.extend(
                self.missing_roles
                    .iter()
                    .map(|m| format!("missing role: {m}")),
            );
            for engine in M8_ENGINES {
                if !self.cells.iter().any(|c| c.engine_kind == engine) {
                    out.push(format!("no {engine} fixture ran the M8 cells"));
                }
            }
        }
        out
    }

    /// The per-engine table, the transfer matrix and one detail line per cell.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "M8 conformance ({REQUIRE_ENV}: {}): + pass, X fail, p pending, s skipped, - n/a",
            self.require_m8
        );
        let width = self
            .engines
            .iter()
            .map(String::len)
            .max()
            .unwrap_or(6)
            .max(6);
        let _ = write!(out, "{:width$}", "engine");
        for check in M8Check::PER_ENGINE {
            let _ = write!(out, "  {:>14}", check.as_str());
        }
        out.push('\n');
        for engine in &self.engines {
            let _ = write!(out, "{engine:width$}");
            for check in M8Check::PER_ENGINE {
                let glyph = self
                    .cell(engine, check, None)
                    .map_or(' ', |c| c.status.glyph());
                let _ = write!(out, "  {glyph:>14}");
            }
            out.push('\n');
        }
        let _ = writeln!(out, "\nTransfer matrix (row: source, column: target):");
        let _ = write!(out, "{:width$}", "source");
        for target in &self.engines {
            let _ = write!(out, "  {target:>8}");
        }
        out.push('\n');
        for source in &self.engines {
            let _ = write!(out, "{source:width$}");
            for target in &self.engines {
                let glyph = if source == target {
                    '-'
                } else {
                    self.cell(source, M8Check::Transfer, Some(target))
                        .map_or(' ', |c| c.status.glyph())
                };
                let _ = write!(out, "  {glyph:>8}");
            }
            out.push('\n');
        }
        let _ = writeln!(out, "\nCells:");
        for c in &self.cells {
            let elapsed = c
                .elapsed_ms
                .map(|ms| format!(" ({ms} ms)"))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "  [{}] {}: {}{elapsed}",
                c.status.label(),
                c.label(),
                c.detail
            );
        }
        if !self.missing_roles.is_empty() {
            let _ = writeln!(out, "\nMissing M8 roles: {}", self.missing_roles.join("; "));
        }
        let s = &self.summary;
        let _ = writeln!(
            out,
            "\nM8 summary: {} cells, {} pass, {} fail, {} pending, {} skipped",
            s.total, s.pass, s.fail, s.pending, s.skipped
        );
        out
    }

    /// Writes the report as pretty JSON (through a temp file).
    pub fn write_json(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        json.push('\n');
        let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(format!(".{}.tmp", std::process::id()));
        let tmp = path.with_file_name(tmp_name);
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }
}

/// The M8 fixtures of `engines`, in [`M8_ENGINES`] order.
pub fn m8_engines(engines: &[ResolvedEngine]) -> Vec<&ResolvedEngine> {
    M8_ENGINES
        .iter()
        .flat_map(|k| engines.iter().filter(move |e| e.engine == *k))
        .collect()
}

/// `missing_m8_roles` of every M8 engine.
pub fn missing_roles(registry: &DialectRegistry) -> Vec<String> {
    M8_ENGINES
        .iter()
        .flat_map(|&e| registry.missing_m8_roles(e))
        .collect()
}

/// What structure sync still lacks on `engine`: the missing roles except the type mapper.
pub fn missing_sync_roles(registry: &DialectRegistry, engine: Engine) -> Vec<String> {
    registry
        .missing_m8_roles(engine)
        .into_iter()
        .filter(|m| !m.ends_with("no TypeMapper"))
        .collect()
}

/// Every cell with the same status and detail (skips, unreachable fixtures).
fn all_cells(engines: &[&ResolvedEngine], status: CellStatus, detail: &str) -> Vec<M8Cell> {
    let mut cells: Vec<M8Cell> = engines
        .iter()
        .flat_map(|e| {
            M8Check::PER_ENGINE
                .into_iter()
                .map(|check| M8Cell::new(e, check, status, detail))
        })
        .collect();
    for s in engines {
        for t in engines {
            if s.name != t.name {
                cells.push(M8Cell::transfer(s, t, status, detail));
            }
        }
    }
    cells
}

/// Loads the fixtures at `path` and runs the M8 cells.
pub fn run_path(path: &Path, opts: M8Options) -> Result<M8Report, FixturesError> {
    let fixtures = Fixtures::from_path(path)?;
    let engines = fixtures.resolve()?;
    Ok(run(&fixtures, &engines, &adapter::registry(), opts))
}

/// Runs every M8 cell of the M8 engines in `engines` (sequentially, on its own tokio runtime:
/// must not be called from inside one).
pub fn run(
    fixtures: &Fixtures,
    engines: &[ResolvedEngine],
    registry: &DialectRegistry,
    opts: M8Options,
) -> M8Report {
    let selected = m8_engines(engines);
    let missing = missing_roles(registry);
    if opts.skip {
        let cells = all_cells(&selected, CellStatus::Skipped, "M8 checks disabled");
        return M8Report::new(&selected, cells, missing, opts.require);
    }
    if fixtures.m8.is_none() {
        let cells = all_cells(
            &selected,
            CellStatus::Pending,
            "pending (tests/fixtures.toml has no [m8])",
        );
        return M8Report::new(&selected, cells, missing, opts.require);
    }
    let cells = live::run(fixtures, &selected, registry, opts);
    M8Report::new(&selected, cells, missing, opts.require)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::Target;
    use pretty_assertions::assert_eq;

    fn engine(name: &str, kind: Engine) -> ResolvedEngine {
        ResolvedEngine {
            name: name.to_owned(),
            engine: kind,
            target: Target::Tcp {
                host: "127.0.0.1".to_owned(),
                port: 1,
            },
            version_prefix: None,
        }
    }

    #[test]
    fn skipped_run_has_every_cell_and_renders() {
        let engines = vec![
            engine("sqlite", Engine::Sqlite),
            engine("postgres", Engine::Postgres),
            engine("redis", Engine::Redis),
            engine("mysql", Engine::MySql),
        ];
        let fixtures = Fixtures::parse("schema_version = 1\n", "t").unwrap();
        let opts = M8Options {
            skip: true,
            ..M8Options::default()
        };
        let report = run(&fixtures, &engines, &adapter::provisional_registry(), opts);
        // M8_ENGINES order, Redis left out.
        assert_eq!(report.engines, vec!["postgres", "mysql", "sqlite"]);
        // 3 engines x 4 checks + 3 x 2 transfers.
        assert_eq!(report.cells.len(), 12 + 6);
        assert_eq!(report.summary.skipped, 18);
        assert_eq!(report.problems(opts), Vec::<String>::new());
        let require = M8Options {
            require: true,
            ..opts
        };
        let problems = report.problems(require);
        assert!(
            problems.iter().any(|p| p.contains("postgres seed skipped")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("no mssql fixture")),
            "{problems:?}"
        );
        let text = report.render();
        assert!(text.contains("Transfer matrix"), "{text}");
        assert!(
            text.contains("[skipped] postgres -> mysql transfer: M8 checks disabled"),
            "{text}"
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["cells"][0]["check"], "seed");
        assert_eq!(json["cells"][0]["status"], "skipped");
    }

    #[test]
    fn missing_m8_table_is_pending_and_fails_only_the_guard() {
        let engines = vec![engine("mariadb", Engine::MariaDb)];
        let fixtures = Fixtures::parse("schema_version = 1\n", "t").unwrap();
        let report = run(
            &fixtures,
            &engines,
            &adapter::provisional_registry(),
            M8Options::default(),
        );
        assert_eq!(report.summary.pending, 4);
        assert_eq!(report.problems(M8Options::default()), Vec::<String>::new());
        let require = M8Options {
            require: true,
            ..M8Options::default()
        };
        assert!(report.problems(require).len() >= 4);
    }

    #[test]
    fn failed_cells_always_block() {
        let e = engine("postgres", Engine::Postgres);
        let mut cells = all_cells(&[&e], CellStatus::Pass, "ok");
        cells[1].status = CellStatus::Fail;
        cells[1].detail = "boom".to_owned();
        let report = M8Report::new(&[&e], cells, Vec::new(), false);
        assert_eq!(
            report.problems(M8Options::default()),
            vec!["postgres sync_structure fail: boom".to_owned()]
        );
    }

    /// The real `tests/fixtures.toml` names every M8 engine and the shapes the cells read.
    #[test]
    fn real_fixtures_have_the_m8_tables() {
        let fx = Fixtures::from_path(&crate::fixtures::fixtures_path()).unwrap();
        let m8 = fx.m8.as_ref().expect("[m8]");
        for engine in M8_ENGINES {
            let (_, seed) = fx.m8_seed(engine.as_str(), engine).expect("[m8.<engine>]");
            if engine == Engine::Sqlite {
                assert!(seed.source_sql.is_some() && seed.target_sql.is_some());
            } else {
                assert!(seed.init_sql.is_some(), "{engine}");
            }
        }
        assert_eq!(m8.data_expected.get("m8_data"), Some(&vec![2, 5, 2]));
        assert_eq!(m8.big_rows, Some(100_000));
        assert!(m8.data_tables.iter().any(|t| t == "m8_nokey"));
        assert!(!m8.data_expected.contains_key("m8_nokey"));
        let transfer = m8.transfer().expect("[m8.transfer]");
        assert_eq!(
            transfer.expected_mysql.get("c_bool"),
            Some(&vec!["tinyint(1)".to_owned(), "exact".to_owned()])
        );
        assert!(transfer.tables.iter().any(|t| t == "m8_parent"));
        let (_, pg) = fx.m8_seed("postgres", Engine::Postgres).unwrap();
        assert_eq!(pg.function_signature.as_deref(), Some("integer"));
        let (_, my) = fx.m8_seed("mysql", Engine::MySql).unwrap();
        assert_eq!(my.skip_items, vec!["sequence:invoice_seq".to_owned()]);
    }

    #[test]
    fn sync_roles_leave_the_type_mapper_out() {
        let registry = adapter::registry();
        for engine in M8_ENGINES {
            assert!(
                missing_sync_roles(&registry, engine)
                    .iter()
                    .all(|m| !m.contains("TypeMapper"))
            );
        }
    }
}
