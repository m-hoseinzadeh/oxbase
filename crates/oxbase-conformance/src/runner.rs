//! Runs the matrix: probe every fixture, then fill in the cells.

use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use oxbase_dialect::DialectRegistry;

use crate::adapter::{self, Engine};
use crate::connect::{self, ConnCell, ConnCheck};
use crate::fetch::{self, FetchOutcome};
use crate::fixtures::{Fixtures, FixturesError, ResolvedEngine};
use crate::matrix::{Cell, CellStatus, Check, plan_engine_in};
use crate::probe::{self, ProbeOptions};
use crate::report::{
    CheckInfo, ConnCheckInfo, EngineStatus, REPORT_SCHEMA_VERSION, Report, Summary,
};
use crate::roundtrip;

/// The milestone this harness build implements.
pub const MILESTONE: &str = "M3";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunOptions {
    pub probe: ProbeOptions,
    /// Report unreachable fixtures as skipped instead of failing.
    pub allow_missing: bool,
    /// Skip the M1 connection checks (they are reported as skipped).
    pub skip_connect: bool,
    /// Skip the M2 `fetch_list` cells (cells with a fetcher are reported as skipped).
    pub skip_fetch: bool,
    /// Skip the M3 `roundtrip` and `capabilities` cells (cells that would run are reported as
    /// skipped).
    pub skip_roundtrip: bool,
}

impl RunOptions {
    /// Reads `OXBASE_CONFORMANCE_ALLOW_MISSING`, `OXBASE_CONFORMANCE_PROBE_TIMEOUT_MS`,
    /// `OXBASE_CONFORMANCE_TCP_ONLY`, `OXBASE_CONFORMANCE_SKIP_CONNECT`,
    /// `OXBASE_CONFORMANCE_SKIP_FETCH` and `OXBASE_CONFORMANCE_SKIP_ROUNDTRIP`.
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok();
        let mut opts = Self::from_vars(
            var("OXBASE_CONFORMANCE_ALLOW_MISSING").as_deref(),
            var("OXBASE_CONFORMANCE_PROBE_TIMEOUT_MS").as_deref(),
            var("OXBASE_CONFORMANCE_TCP_ONLY").as_deref(),
        );
        opts.skip_connect = is_truthy(var("OXBASE_CONFORMANCE_SKIP_CONNECT").as_deref());
        opts.skip_fetch = is_truthy(var("OXBASE_CONFORMANCE_SKIP_FETCH").as_deref());
        opts.skip_roundtrip = is_truthy(var("OXBASE_CONFORMANCE_SKIP_ROUNDTRIP").as_deref());
        opts
    }

    /// [`RunOptions::from_env`] with the values passed in.
    pub fn from_vars(
        allow_missing: Option<&str>,
        timeout_ms: Option<&str>,
        tcp_only: Option<&str>,
    ) -> Self {
        let timeout = timeout_ms
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&ms| ms > 0)
            .map_or(probe::DEFAULT_TIMEOUT, Duration::from_millis);
        Self {
            probe: ProbeOptions {
                timeout,
                handshake: !is_truthy(tcp_only),
            },
            allow_missing: is_truthy(allow_missing),
            skip_connect: false,
            skip_fetch: false,
            skip_roundtrip: false,
        }
    }
}

pub(crate) fn is_truthy(v: Option<&str>) -> bool {
    v.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Loads the fixtures file at `path` and runs the matrix.
pub fn run_path(path: &Path, opts: &RunOptions) -> Result<Report, FixturesError> {
    let fixtures = Fixtures::from_path(path)?;
    let engines = fixtures.resolve()?;
    Ok(run(path, &fixtures, &engines, opts))
}

/// Probes every engine concurrently, runs the connection checks, the `fetch_list` cells and the
/// `roundtrip` / `capabilities` cells, and builds the report.
pub fn run(
    fixtures_path: &Path,
    fixtures: &Fixtures,
    engines: &[ResolvedEngine],
    opts: &RunOptions,
) -> Report {
    let probes: Vec<_> = thread::scope(|scope| {
        let handles: Vec<_> = engines
            .iter()
            .map(|e| {
                scope.spawn(move || {
                    probe::probe(e.engine, &e.target, e.version_prefix.as_deref(), opts.probe)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("probe thread panicked"))
            .collect()
    });

    let reachable: Vec<bool> = probes.iter().map(|p| p.reachable).collect();
    let connections = if opts.skip_connect {
        skipped_connections(engines)
    } else {
        connect::run_all(fixtures, engines, &reachable)
    };

    let registry = adapter::registry();
    let fetched = if opts.skip_fetch {
        Vec::new()
    } else {
        fetch::run_all(fixtures, engines, &reachable, &registry)
    };
    let round_trips = if opts.skip_roundtrip {
        Vec::new()
    } else {
        roundtrip::run_all(fixtures, engines, &reachable, &registry)
    };
    let mut statuses = Vec::with_capacity(engines.len());
    let mut cells = Vec::new();
    for (e, probe) in engines.iter().zip(probes) {
        let mut planned = plan_engine_in(&registry, &e.name, e.engine);
        if probe.reachable {
            apply_fetch(&mut planned, &fetched, opts.skip_fetch);
            roundtrip::apply(&mut planned, &round_trips);
            if opts.skip_roundtrip {
                skip_roundtrip(&mut planned, &registry, e.engine);
            }
        } else {
            for cell in &mut planned {
                if cell.status == CellStatus::Pending {
                    cell.status = CellStatus::Skipped;
                    cell.detail = Some("fixture unreachable".to_owned());
                }
            }
        }
        cells.extend(planned);
        statuses.push(EngineStatus {
            name: e.name.clone(),
            kind: e.engine,
            target: e.target.clone(),
            probe,
        });
    }

    let missing_engines = Engine::ALL
        .into_iter()
        .filter(|k| !engines.iter().any(|e| e.engine == *k))
        .collect();

    Report {
        report_schema_version: REPORT_SCHEMA_VERSION,
        milestone: MILESTONE.to_owned(),
        generated_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        fixtures_path: fixtures_path.to_path_buf(),
        compose_project: fixtures.compose_project().map(str::to_owned),
        allow_missing: opts.allow_missing,
        engines: statuses,
        missing_engines,
        checks: Check::ALL
            .iter()
            .map(|&c| CheckInfo {
                id: c,
                description: c.description().to_owned(),
            })
            .collect(),
        summary: Summary::from_cells(&cells),
        cells,
        connection_checks: ConnCheck::ALL
            .iter()
            .map(|&c| ConnCheckInfo {
                id: c,
                description: c.description().to_owned(),
            })
            .collect(),
        connection_summary: Summary::from_statuses(connections.iter().map(|c| c.status)),
        connections,
    }
}

/// Fills the `fetch_list` cells of one engine from the outcomes of [`fetch::run_all`]. With
/// `skip_fetch`, cells that would have run are reported as skipped.
fn apply_fetch(cells: &mut [Cell], fetched: &[FetchOutcome], skip_fetch: bool) {
    for cell in cells.iter_mut().filter(|c| c.check == Check::FetchList) {
        if let Some(o) = fetched
            .iter()
            .find(|o| o.engine == cell.engine && o.kind == cell.object_kind)
        {
            cell.status = o.status;
            cell.detail = Some(o.detail.clone());
            cell.elapsed_ms = o.elapsed_ms;
        } else if skip_fetch
            && cell.status == CellStatus::Pending
            && cell.detail.as_deref() == Some(crate::matrix::PENDING_NOT_RUN)
        {
            cell.status = CellStatus::Skipped;
            cell.detail = Some("fetch checks disabled".to_owned());
        }
    }
}

/// With `skip_roundtrip`, the `roundtrip` / `capabilities` cells that would have run are
/// reported as skipped.
fn skip_roundtrip(cells: &mut [Cell], registry: &DialectRegistry, engine: Engine) {
    let runnable = roundtrip::runnable_kinds(registry, engine);
    for cell in cells.iter_mut().filter(|c| {
        matches!(c.check, Check::Roundtrip | Check::Capabilities)
            && c.status == CellStatus::Pending
            && runnable.contains(&c.object_kind)
    }) {
        cell.status = CellStatus::Skipped;
        cell.detail = Some("round-trip checks disabled".to_owned());
    }
}

/// Connection cells when the checks are switched off: pending/n-a cells stay as they are, the
/// rest are skipped.
fn skipped_connections(engines: &[ResolvedEngine]) -> Vec<ConnCell> {
    engines
        .iter()
        .flat_map(|e| {
            connect::static_cells(e, false)
                .unwrap_or_default()
                .into_iter()
                .map(|mut c| {
                    if c.status == CellStatus::Skipped {
                        c.detail = Some("connection checks disabled".to_owned());
                    }
                    c
                })
        })
        .collect()
}

/// Probes every fixture of one engine kind. `Ok` carries a one-line summary; `Err` names the
/// fixture, endpoint and cause. Unreachable fixtures are `Ok` when `allow_missing` is set.
pub fn check_engine(
    fixtures_path: &Path,
    engine: Engine,
    opts: &RunOptions,
) -> Result<String, String> {
    let fixtures = Fixtures::from_path(fixtures_path).map_err(|e| e.to_string())?;
    let engines: Vec<_> = fixtures
        .resolve()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|e| e.engine == engine)
        .collect();
    if engines.is_empty() {
        return Err(format!(
            "{} has no fixture with kind = \"{engine}\"",
            fixtures_path.display()
        ));
    }
    // Reachability only: the connection checks, fetch and round-trip cells belong to the full
    // run.
    let opts = RunOptions {
        skip_connect: true,
        skip_fetch: true,
        skip_roundtrip: true,
        ..*opts
    };
    let report = run(fixtures_path, &fixtures, &engines, &opts);
    let lines: Vec<_> = report
        .engines
        .iter()
        .map(|e| {
            format!(
                "{} ({}) at {}: {}",
                e.name,
                e.kind,
                e.target,
                e.probe.summary()
            )
        })
        .collect();
    let down = report.unreachable().count();
    if down > 0 && !opts.allow_missing {
        Err(format!(
            "{engine} fixture unreachable:\n  {}\nStart the fixtures with scripts/fixtures-up.sh, \
             or set OXBASE_CONFORMANCE_ALLOW_MISSING=1 to skip.",
            lines.join("\n  ")
        ))
    } else if down > 0 {
        Ok(format!("skipped (allowed missing): {}", lines.join("; ")))
    } else {
        Ok(lines.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;
    use crate::probe::Target;

    #[test]
    fn options_from_vars() {
        let d = RunOptions::from_vars(None, None, None);
        assert_eq!(d, RunOptions::default());
        assert!(!d.allow_missing);
        assert!(d.probe.handshake);

        let o = RunOptions::from_vars(Some("1"), Some("250"), Some("TRUE"));
        assert!(o.allow_missing);
        assert!(!o.probe.handshake);
        assert_eq!(o.probe.timeout, Duration::from_millis(250));

        let o = RunOptions::from_vars(Some("0"), Some("junk"), Some(""));
        assert!(!o.allow_missing);
        assert_eq!(o.probe.timeout, probe::DEFAULT_TIMEOUT);
    }

    /// Object kinds that apply to Redis.
    fn redis_kinds() -> usize {
        oxbase_types::ObjectKind::ALL
            .into_iter()
            .filter(|&k| adapter::supports_kind(Engine::Redis, k))
            .count()
    }

    fn closed_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    #[test]
    fn unreachable_engine_cells_become_skipped() {
        let text = format!(
            "schema_version = 1\n[engines.redis]\nkind = \"redis\"\nport = {}\n",
            closed_port()
        );
        let fixtures = Fixtures::parse(&text, "t").unwrap();
        let engines = fixtures.resolve().unwrap();
        let opts = RunOptions::from_vars(Some("1"), Some("500"), None);
        let report = run(Path::new("t"), &fixtures, &engines, &opts);

        assert!(!report.engines[0].probe.reachable);
        // Redis has no driver before M7: its connection checks are pending, never run.
        assert_eq!(report.connection_summary.pending, ConnCheck::ALL.len());
        assert_eq!(report.summary.pending, 0);
        assert_eq!(report.summary.skipped, redis_kinds() * Check::ALL.len());
        assert_eq!(report.missing_engines.len(), Engine::ALL.len() - 1);
        // Missing engines fail the gate even with allow_missing.
        assert!(report.gate().is_err());
    }

    #[test]
    fn reachable_engine_cells_are_pending() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let engines = vec![ResolvedEngine {
            name: "r".to_owned(),
            engine: Engine::Redis,
            target: Target::Tcp {
                host: "127.0.0.1".to_owned(),
                port,
            },
            version_prefix: None,
        }];
        let fixtures = Fixtures::parse("schema_version = 1\n", "t").unwrap();
        let opts = RunOptions::from_vars(None, Some("500"), Some("1"));
        let report = run(Path::new("t"), &fixtures, &engines, &opts);
        assert!(report.engines[0].probe.reachable);
        assert_eq!(report.summary.pending, redis_kinds() * Check::ALL.len());
        assert_eq!(report.summary.skipped, 0);
        assert_eq!(report.milestone, MILESTONE);
    }

    fn fetch_cell(kind: oxbase_types::ObjectKind, detail: &str) -> Cell {
        Cell {
            engine: "lite".to_owned(),
            engine_kind: Engine::Sqlite,
            object_kind: kind,
            check: Check::FetchList,
            status: CellStatus::Pending,
            detail: Some(detail.to_owned()),
            elapsed_ms: None,
        }
    }

    #[test]
    fn fetch_outcomes_fill_their_cells_only() {
        use oxbase_types::ObjectKind as K;
        let mut cells = vec![
            fetch_cell(K::Table, crate::matrix::PENDING_NOT_RUN),
            fetch_cell(K::View, crate::matrix::PENDING_NOT_RUN),
            fetch_cell(K::Trigger, crate::matrix::PENDING_NO_FETCHER),
        ];
        let fetched = vec![
            FetchOutcome {
                engine: "lite".to_owned(),
                kind: K::Table,
                status: CellStatus::Pass,
                detail: "ok".to_owned(),
                elapsed_ms: Some(3),
            },
            FetchOutcome {
                engine: "other".to_owned(),
                kind: K::View,
                status: CellStatus::Fail,
                detail: "not ours".to_owned(),
                elapsed_ms: None,
            },
        ];
        apply_fetch(&mut cells, &fetched, false);
        assert_eq!(cells[0].status, CellStatus::Pass);
        assert_eq!(cells[0].elapsed_ms, Some(3));
        assert_eq!(cells[1].status, CellStatus::Pending);
        assert_eq!(cells[2].status, CellStatus::Pending);

        apply_fetch(&mut cells, &[], true);
        assert_eq!(cells[0].status, CellStatus::Pass);
        assert_eq!(cells[1].status, CellStatus::Skipped);
        assert_eq!(cells[1].detail.as_deref(), Some("fetch checks disabled"));
        // No fetcher: nothing would have run, so it stays pending.
        assert_eq!(cells[2].status, CellStatus::Pending);
    }
}
