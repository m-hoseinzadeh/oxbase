//! The object-kind x engine matrix of dialect implementations.

use std::collections::{HashMap, HashSet};
use std::fmt::{self, Write as _};
use std::sync::Arc;

use oxbase_types::{Engine, Error, ObjectKind, Result};

use crate::{Capabilities, Comparator, DdlGenerator, DmlWriter, Fetcher, Quoting};

/// Roles filled per (engine, object kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CellRole {
    Fetcher,
    DdlGenerator,
    Comparator,
}

impl CellRole {
    pub const ALL: [CellRole; 3] = [
        CellRole::Fetcher,
        CellRole::DdlGenerator,
        CellRole::Comparator,
    ];

    /// One-letter code used in the matrix (`F`, `D`, `C`).
    pub const fn letter(self) -> char {
        match self {
            Self::Fetcher => 'F',
            Self::DdlGenerator => 'D',
            Self::Comparator => 'C',
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Fetcher => "Fetcher",
            Self::DdlGenerator => "DdlGenerator",
            Self::Comparator => "Comparator",
        }
    }
}

/// Roles filled once per engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EngineRole {
    Capabilities,
    Quoting,
    DmlWriter,
}

impl EngineRole {
    pub const ALL: [EngineRole; 3] = [
        EngineRole::Capabilities,
        EngineRole::Quoting,
        EngineRole::DmlWriter,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Capabilities => "Capabilities",
            Self::Quoting => "Quoting",
            Self::DmlWriter => "DmlWriter",
        }
    }
}

/// A role that has no implementation for an (engine, kind) cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MissingCell {
    pub engine: Engine,
    pub kind: ObjectKind,
    pub role: CellRole,
}

impl fmt::Display for MissingCell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}: no {}", self.engine, self.kind, self.role.name())
    }
}

/// Coverage of one (engine, kind) cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellStatus {
    /// The kind does not exist on this engine (declared with
    /// [`DialectRegistry::mark_not_applicable`]) and nothing is registered for it.
    NotApplicable,
    /// The roles that are registered, in [`CellRole::ALL`] order (empty = nothing yet).
    Roles(Vec<CellRole>),
}

impl CellStatus {
    /// `true` when nothing more is needed: not applicable, or every role registered.
    pub fn is_complete(&self) -> bool {
        match self {
            Self::NotApplicable => true,
            Self::Roles(r) => r.len() == CellRole::ALL.len(),
        }
    }

    /// Matrix text: `n/a`, or one letter per role with `-` for missing roles (`F-C`).
    pub fn code(&self) -> String {
        match self {
            Self::NotApplicable => "n/a".to_owned(),
            Self::Roles(present) => CellRole::ALL
                .iter()
                .map(|r| if present.contains(r) { r.letter() } else { '-' })
                .collect(),
        }
    }
}

/// The object-kind x engine coverage grid, printable as a text table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageMatrix {
    pub engines: Vec<Engine>,
    pub kinds: Vec<ObjectKind>,
    /// `cells[k][e]` is the status of `kinds[k]` on `engines[e]`.
    pub cells: Vec<Vec<CellStatus>>,
    /// `engine_roles[e]` lists the engine-wide roles registered for `engines[e]`.
    pub engine_roles: Vec<Vec<EngineRole>>,
}

impl CoverageMatrix {
    /// Status of one cell, if both the engine and the kind are in the matrix.
    pub fn get(&self, engine: Engine, kind: ObjectKind) -> Option<&CellStatus> {
        let e = self.engines.iter().position(|x| *x == engine)?;
        let k = self.kinds.iter().position(|x| *x == kind)?;
        Some(&self.cells[k][e])
    }

    /// Number of cells that are not complete.
    pub fn incomplete(&self) -> usize {
        self.cells
            .iter()
            .flatten()
            .filter(|c| !c.is_complete())
            .count()
    }
}

impl fmt::Display for CoverageMatrix {
    /// Rows are object kinds, columns engines. Cells read `FDC` (Fetcher, DdlGenerator,
    /// Comparator) with `-` for a missing role, or `n/a`. Engine-wide roles follow as `yes`/`-`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label_w = self
            .kinds
            .iter()
            .map(|k| k.as_str().len())
            .chain(EngineRole::ALL.iter().map(|r| r.name().len()))
            .max()
            .unwrap_or(0);
        let col_w: Vec<usize> = self
            .engines
            .iter()
            .map(|e| e.as_str().len().max(3))
            .collect();

        // Each line is built then right-trimmed, so the output has no trailing spaces.
        let mut emit = |label: &str, cells: &mut dyn Iterator<Item = String>| -> fmt::Result {
            let mut line = format!("{label:label_w$}");
            for (cell, w) in cells.zip(&col_w) {
                let _ = write!(line, "  {cell:w$}");
            }
            writeln!(f, "{}", line.trim_end())
        };
        emit("", &mut self.engines.iter().map(|e| e.as_str().to_owned()))?;
        for (k, row) in self.kinds.iter().zip(&self.cells) {
            emit(k.as_str(), &mut row.iter().map(CellStatus::code))?;
        }
        for role in EngineRole::ALL {
            let marks = self
                .engine_roles
                .iter()
                .map(|roles| if roles.contains(&role) { "yes" } else { "-" }.to_owned());
            emit(role.name(), &mut marks.into_iter())?;
        }
        Ok(())
    }
}

#[derive(Default, Clone)]
struct Cell {
    fetcher: Option<Arc<dyn Fetcher>>,
    ddl: Option<Arc<dyn DdlGenerator>>,
    comparator: Option<Arc<dyn Comparator>>,
}

impl Cell {
    fn roles(&self) -> Vec<CellRole> {
        let mut r = Vec::new();
        if self.fetcher.is_some() {
            r.push(CellRole::Fetcher);
        }
        if self.ddl.is_some() {
            r.push(CellRole::DdlGenerator);
        }
        if self.comparator.is_some() {
            r.push(CellRole::Comparator);
        }
        r
    }
}

#[derive(Default, Clone)]
struct EngineEntry {
    capabilities: Option<Arc<dyn Capabilities>>,
    quoting: Option<Arc<dyn Quoting>>,
    dml: Option<Arc<dyn DmlWriter>>,
}

/// Holds every dialect implementation, keyed by (engine, object kind) for per-kind roles and by
/// engine for engine-wide roles.
///
/// Engine crates register their cells at startup; services look them up. Lookups of a missing
/// role fail with `Error::Unsupported`, so a half-filled engine degrades instead of panicking.
#[derive(Default, Clone)]
pub struct DialectRegistry {
    cells: HashMap<(Engine, ObjectKind), Cell>,
    engines: HashMap<Engine, EngineEntry>,
    not_applicable: HashSet<(Engine, ObjectKind)>,
}

impl DialectRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn cell_mut(&mut self, engine: Engine, kind: ObjectKind) -> &mut Cell {
        self.cells.entry((engine, kind)).or_default()
    }

    fn engine_mut(&mut self, engine: Engine) -> &mut EngineEntry {
        self.engines.entry(engine).or_default()
    }

    // --- registration ---

    pub fn register_fetcher(
        &mut self,
        engine: Engine,
        kind: ObjectKind,
        fetcher: Arc<dyn Fetcher>,
    ) -> &mut Self {
        self.cell_mut(engine, kind).fetcher = Some(fetcher);
        self
    }

    pub fn register_ddl(
        &mut self,
        engine: Engine,
        kind: ObjectKind,
        ddl: Arc<dyn DdlGenerator>,
    ) -> &mut Self {
        self.cell_mut(engine, kind).ddl = Some(ddl);
        self
    }

    pub fn register_comparator(
        &mut self,
        engine: Engine,
        kind: ObjectKind,
        comparator: Arc<dyn Comparator>,
    ) -> &mut Self {
        self.cell_mut(engine, kind).comparator = Some(comparator);
        self
    }

    pub fn register_capabilities(
        &mut self,
        engine: Engine,
        capabilities: Arc<dyn Capabilities>,
    ) -> &mut Self {
        self.engine_mut(engine).capabilities = Some(capabilities);
        self
    }

    pub fn register_quoting(&mut self, engine: Engine, quoting: Arc<dyn Quoting>) -> &mut Self {
        self.engine_mut(engine).quoting = Some(quoting);
        self
    }

    pub fn register_dml_writer(&mut self, engine: Engine, dml: Arc<dyn DmlWriter>) -> &mut Self {
        self.engine_mut(engine).dml = Some(dml);
        self
    }

    /// Declares that `kind` does not exist on `engine`, so its empty cell is not reported as
    /// missing. Registering any role for the cell later overrides this.
    pub fn mark_not_applicable(&mut self, engine: Engine, kind: ObjectKind) -> &mut Self {
        self.not_applicable.insert((engine, kind));
        self
    }

    // --- lookup ---

    fn missing(engine: Engine, kind: ObjectKind, role: CellRole) -> Error {
        Error::unsupported(format!(
            "{} has no {} for {}",
            engine.display_name(),
            role.name(),
            kind.display_name()
        ))
    }

    fn missing_engine(engine: Engine, role: EngineRole) -> Error {
        Error::unsupported(format!("{} has no {}", engine.display_name(), role.name()))
    }

    pub fn fetcher(&self, engine: Engine, kind: ObjectKind) -> Result<Arc<dyn Fetcher>> {
        self.cells
            .get(&(engine, kind))
            .and_then(|c| c.fetcher.clone())
            .ok_or_else(|| Self::missing(engine, kind, CellRole::Fetcher))
    }

    pub fn ddl(&self, engine: Engine, kind: ObjectKind) -> Result<Arc<dyn DdlGenerator>> {
        self.cells
            .get(&(engine, kind))
            .and_then(|c| c.ddl.clone())
            .ok_or_else(|| Self::missing(engine, kind, CellRole::DdlGenerator))
    }

    pub fn comparator(&self, engine: Engine, kind: ObjectKind) -> Result<Arc<dyn Comparator>> {
        self.cells
            .get(&(engine, kind))
            .and_then(|c| c.comparator.clone())
            .ok_or_else(|| Self::missing(engine, kind, CellRole::Comparator))
    }

    pub fn capabilities(&self, engine: Engine) -> Result<Arc<dyn Capabilities>> {
        self.engines
            .get(&engine)
            .and_then(|e| e.capabilities.clone())
            .ok_or_else(|| Self::missing_engine(engine, EngineRole::Capabilities))
    }

    pub fn quoting(&self, engine: Engine) -> Result<Arc<dyn Quoting>> {
        self.engines
            .get(&engine)
            .and_then(|e| e.quoting.clone())
            .ok_or_else(|| Self::missing_engine(engine, EngineRole::Quoting))
    }

    pub fn dml_writer(&self, engine: Engine) -> Result<Arc<dyn DmlWriter>> {
        self.engines
            .get(&engine)
            .and_then(|e| e.dml.clone())
            .ok_or_else(|| Self::missing_engine(engine, EngineRole::DmlWriter))
    }

    // --- coverage ---

    /// Coverage of one cell.
    pub fn status(&self, engine: Engine, kind: ObjectKind) -> CellStatus {
        let roles = self
            .cells
            .get(&(engine, kind))
            .map(Cell::roles)
            .unwrap_or_default();
        if roles.is_empty() && self.not_applicable.contains(&(engine, kind)) {
            CellStatus::NotApplicable
        } else {
            CellStatus::Roles(roles)
        }
    }

    /// Every missing per-kind role for `engines` across [`ObjectKind::ALL`], skipping
    /// not-applicable cells. Sorted by engine, kind, role.
    pub fn missing_cells(&self, engines: &[Engine]) -> Vec<MissingCell> {
        let mut out = Vec::new();
        for &engine in engines {
            for kind in ObjectKind::ALL {
                if let CellStatus::Roles(present) = self.status(engine, kind) {
                    out.extend(
                        CellRole::ALL
                            .into_iter()
                            .filter(|r| !present.contains(r))
                            .map(|role| MissingCell { engine, kind, role }),
                    );
                }
            }
        }
        out.sort();
        out
    }

    /// Engine-wide roles registered for `engine`, in [`EngineRole::ALL`] order.
    pub fn engine_roles(&self, engine: Engine) -> Vec<EngineRole> {
        let Some(e) = self.engines.get(&engine) else {
            return Vec::new();
        };
        let mut r = Vec::new();
        if e.capabilities.is_some() {
            r.push(EngineRole::Capabilities);
        }
        if e.quoting.is_some() {
            r.push(EngineRole::Quoting);
        }
        if e.dml.is_some() {
            r.push(EngineRole::DmlWriter);
        }
        r
    }

    /// Missing engine-wide roles for `engines`.
    pub fn missing_engine_roles(&self, engines: &[Engine]) -> Vec<(Engine, EngineRole)> {
        engines
            .iter()
            .flat_map(|&engine| {
                let present = self.engine_roles(engine);
                EngineRole::ALL
                    .into_iter()
                    .filter(move |r| !present.contains(r))
                    .map(move |r| (engine, r))
            })
            .collect()
    }

    /// The coverage grid for `engines` x [`ObjectKind::ALL`].
    pub fn matrix(&self, engines: &[Engine]) -> CoverageMatrix {
        let kinds = ObjectKind::ALL.to_vec();
        let cells = kinds
            .iter()
            .map(|&k| engines.iter().map(|&e| self.status(e, k)).collect())
            .collect();
        CoverageMatrix {
            engines: engines.to_vec(),
            kinds,
            cells,
            engine_roles: engines.iter().map(|&e| self.engine_roles(e)).collect(),
        }
    }
}

impl fmt::Debug for DialectRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut cells: Vec<String> = self
            .cells
            .iter()
            .map(|((e, k), c)| format!("{e}/{k}:{}", CellStatus::Roles(c.roles()).code()))
            .collect();
        cells.sort();
        f.debug_struct("DialectRegistry")
            .field("cells", &cells)
            .finish_non_exhaustive()
    }
}
