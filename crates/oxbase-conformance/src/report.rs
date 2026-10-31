//! The run report: a readable table for stdout and a JSON file for tooling.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::adapter::{Engine, ObjectKind};
use crate::fixtures::in_workspace;
use crate::matrix::{Cell, CellStatus, Check};
use crate::probe::{ProbeOutcome, Target};

/// Bumped when the JSON layout changes incompatibly.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// `<CARGO_TARGET_DIR or <workspace>/target>/conformance/report.json`.
pub fn default_report_path() -> PathBuf {
    report_path("report.json")
}

/// `<target>/conformance/<file_name>`. A relative `CARGO_TARGET_DIR` is resolved against the
/// current directory, as cargo does.
pub fn report_path(file_name: &str) -> PathBuf {
    target_dir(
        std::env::var_os("CARGO_TARGET_DIR"),
        std::env::current_dir().ok(),
    )
    .join("conformance")
    .join(file_name)
}

fn target_dir(env: Option<std::ffi::OsString>, cwd: Option<PathBuf>) -> PathBuf {
    match env.filter(|v| !v.is_empty()).map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        Some(dir) => cwd.map_or_else(|| in_workspace(&dir), |cwd| cwd.join(&dir)),
        None => in_workspace("target"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineStatus {
    /// The fixture name (`[engines.<name>]`).
    pub name: String,
    pub kind: Engine,
    pub target: Target,
    pub probe: ProbeOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckInfo {
    pub id: Check,
    pub description: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub total: usize,
    pub pass: usize,
    pub fail: usize,
    pub pending: usize,
    pub skipped: usize,
    pub not_applicable: usize,
}

impl Summary {
    pub fn from_cells(cells: &[Cell]) -> Self {
        let mut s = Self {
            total: cells.len(),
            ..Self::default()
        };
        for c in cells {
            *match c.status {
                CellStatus::Pass => &mut s.pass,
                CellStatus::Fail => &mut s.fail,
                CellStatus::Pending => &mut s.pending,
                CellStatus::Skipped => &mut s.skipped,
                CellStatus::NotApplicable => &mut s.not_applicable,
            } += 1;
        }
        s
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub report_schema_version: u32,
    pub milestone: String,
    pub generated_at_unix: u64,
    pub fixtures_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose_project: Option<String>,
    pub allow_missing: bool,
    pub engines: Vec<EngineStatus>,
    /// v1 engines with no entry in the fixtures file. Always a failure.
    pub missing_engines: Vec<Engine>,
    pub checks: Vec<CheckInfo>,
    pub cells: Vec<Cell>,
    pub summary: Summary,
}

impl Report {
    pub fn unreachable(&self) -> impl Iterator<Item = &EngineStatus> {
        self.engines.iter().filter(|e| !e.probe.reachable)
    }

    /// `Err` with a message naming every engine and endpoint that blocks the run: engines
    /// missing from the fixtures file, failed cells, and (unless `allow_missing`) unreachable
    /// fixtures.
    pub fn gate(&self) -> Result<(), String> {
        let mut problems = Vec::new();
        if !self.missing_engines.is_empty() {
            let names: Vec<_> = self.missing_engines.iter().map(|e| e.as_str()).collect();
            problems.push(format!(
                "{} has no fixture for: {}",
                self.fixtures_path.display(),
                names.join(", ")
            ));
        }
        if self.summary.fail > 0 {
            problems.push(format!("{} matrix cell(s) failed", self.summary.fail));
        }
        if !self.allow_missing {
            let down: Vec<_> = self
                .unreachable()
                .map(|e| {
                    format!(
                        "  - {} ({}) at {}: {}",
                        e.name,
                        e.kind,
                        e.target,
                        e.probe.error.as_deref().unwrap_or("unreachable")
                    )
                })
                .collect();
            if !down.is_empty() {
                let project = self.compose_project.as_deref().unwrap_or("oxbase-fixtures");
                problems.push(format!(
                    "{} fixture(s) unreachable:\n{}\nStart them with scripts/fixtures-up.sh \
                     (compose project {project}), or set OXBASE_CONFORMANCE_ALLOW_MISSING=1 to \
                     report them as skipped instead of failing.",
                    down.len(),
                    down.join("\n")
                ));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(format!("conformance run failed:\n{}", problems.join("\n")))
        }
    }

    /// Writes pretty JSON to `path`, creating parent directories. The JSON goes to a temporary
    /// file in the same directory that is then renamed over `path`, so a reader never sees a
    /// half-written report. Concurrent writers to the same path still race: the last rename wins.
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

    fn status(&self, engine: &str, kind: ObjectKind, check: Check) -> Option<CellStatus> {
        self.cells
            .iter()
            .find(|c| c.engine == engine && c.object_kind == kind && c.check == check)
            .map(|c| c.status)
    }

    /// The human-readable report: engine reachability, then the object kind x engine matrix.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "oxbase conformance: milestone {}, {} engine(s), fixtures {}",
            self.milestone,
            self.engines.len(),
            self.fixtures_path.display()
        );
        out.push('\n');

        let rows: Vec<Vec<String>> = self
            .engines
            .iter()
            .map(|e| {
                let status = if e.probe.reachable {
                    "reachable"
                } else if self.allow_missing {
                    "UNREACHABLE (allowed)"
                } else {
                    "UNREACHABLE"
                };
                vec![
                    e.name.clone(),
                    e.kind.to_string(),
                    e.target.to_string(),
                    status.to_owned(),
                    e.probe.summary(),
                ]
            })
            .collect();
        write_table(
            &mut out,
            &["ENGINE", "KIND", "ENDPOINT", "STATUS", "DETAIL"],
            &rows,
        );
        for missing in &self.missing_engines {
            let _ = writeln!(out, "{missing:<8}  (no fixture defined)");
        }
        let unchecked: Vec<_> = self
            .engines
            .iter()
            .filter(|e| e.probe.version_not_checked())
            .map(|e| e.name.as_str())
            .collect();
        if !unchecked.is_empty() {
            let _ = writeln!(
                out,
                "Version not checked for: {} (the probe gets no version from the server; \
                 the version_prefix in the fixtures file is not verified).",
                unchecked.join(", ")
            );
        }
        out.push('\n');

        let checks: Vec<_> = self.checks.iter().map(|c| c.id.as_str()).collect();
        let _ = writeln!(out, "Matrix: each cell is {}.", checks.join(" / "));
        let legend: Vec<_> = CellStatus::ALL
            .iter()
            .map(|s| format!("{}={}", s.glyph(), s.label()))
            .collect();
        let _ = writeln!(out, "Legend: {}", legend.join("  "));
        let mut header = vec!["OBJECT KIND"];
        header.extend(self.engines.iter().map(|e| e.name.as_str()));
        let rows: Vec<Vec<String>> = ObjectKind::ALL
            .iter()
            .map(|&kind| {
                let mut row = vec![kind.to_string()];
                for e in &self.engines {
                    row.push(
                        self.checks
                            .iter()
                            .map(|c| {
                                self.status(&e.name, kind, c.id)
                                    .map_or('?', CellStatus::glyph)
                            })
                            .collect(),
                    );
                }
                row
            })
            .collect();
        write_table(&mut out, &header, &rows);
        out.push('\n');

        let s = &self.summary;
        let _ = writeln!(
            out,
            "Summary: {} cells: {} pass, {} fail, {} pending, {} skipped, {} n/a",
            s.total, s.pass, s.fail, s.pending, s.skipped, s.not_applicable
        );
        if s.pending > 0 && s.pass == 0 && s.fail == 0 {
            let _ = writeln!(
                out,
                "All applicable cells are pending: no engine has a driver yet."
            );
        }
        out
    }
}

/// Left-aligned columns separated by two spaces, trailing whitespace trimmed.
fn write_table<S: AsRef<str>>(out: &mut String, header: &[&str], rows: &[Vec<S>]) {
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            let n = cell.as_ref().chars().count();
            match widths.get_mut(i) {
                Some(w) => *w = (*w).max(n),
                None => widths.push(n),
            }
        }
    }
    let mut line = |cells: &mut dyn Iterator<Item = &str>| {
        let mut s = String::new();
        for (i, cell) in cells.enumerate() {
            if i > 0 {
                s.push_str("  ");
            }
            let _ = write!(s, "{cell:<width$}", width = widths[i]);
        }
        out.push_str(s.trim_end());
        out.push('\n');
    };
    line(&mut header.iter().copied());
    for row in rows {
        line(&mut row.iter().map(AsRef::as_ref));
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::matrix::plan_engine;
    use crate::probe::Handshake;

    fn probe(reachable: bool) -> ProbeOutcome {
        ProbeOutcome {
            reachable,
            latency_ms: 2,
            handshake: if reachable {
                Handshake::Verified {
                    detail: "ok".to_owned(),
                }
            } else {
                Handshake::NotAttempted
            },
            server_version: None,
            version_matches: None,
            expected_version: None,
            error: (!reachable).then(|| "TCP connect failed: refused".to_owned()),
        }
    }

    fn sample(redis_up: bool, allow_missing: bool) -> Report {
        let engines = vec![
            EngineStatus {
                name: "postgres".to_owned(),
                kind: Engine::Postgres,
                target: Target::Tcp {
                    host: "127.0.0.1".to_owned(),
                    port: 15432,
                },
                probe: probe(true),
            },
            EngineStatus {
                name: "redis".to_owned(),
                kind: Engine::Redis,
                target: Target::Tcp {
                    host: "127.0.0.1".to_owned(),
                    port: 15479,
                },
                probe: probe(redis_up),
            },
        ];
        let mut cells = plan_engine("postgres", Engine::Postgres);
        cells.extend(plan_engine("redis", Engine::Redis));
        let summary = Summary::from_cells(&cells);
        Report {
            report_schema_version: REPORT_SCHEMA_VERSION,
            milestone: "M0".to_owned(),
            generated_at_unix: 0,
            fixtures_path: PathBuf::from("/repo/tests/fixtures.toml"),
            compose_project: Some("oxbase-fixtures".to_owned()),
            allow_missing,
            engines,
            missing_engines: vec![],
            checks: Check::ALL
                .iter()
                .map(|&c| CheckInfo {
                    id: c,
                    description: c.description().to_owned(),
                })
                .collect(),
            cells,
            summary,
        }
    }

    fn line_starting<'a>(text: &'a str, prefix: &str) -> &'a str {
        text.lines()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no line starting with {prefix:?} in:\n{text}"))
    }

    #[test]
    fn renders_matrix_rows_with_glyphs() {
        let text = sample(true, false).render();
        let header = line_starting(&text, "OBJECT KIND");
        assert_eq!(header, "OBJECT KIND  postgres  redis");
        assert_eq!(line_starting(&text, "table "), "table        ppp       ---");
        assert_eq!(line_starting(&text, "key "), "key          ---       ppp");
        assert_eq!(line_starting(&text, "user "), "user         ppp       ppp");
        assert!(text.contains("Matrix: each cell is roundtrip / fetch_list / capabilities."));
        assert!(text.contains("p=pending"));
        // Every object kind has a row.
        for kind in ObjectKind::ALL {
            line_starting(&text, &format!("{kind} "));
        }
    }

    #[test]
    fn renders_engine_table_and_summary() {
        let text = sample(true, false).render();
        let pg = line_starting(&text, "postgres ");
        assert!(pg.contains("127.0.0.1:15432"), "{pg}");
        assert!(pg.contains("reachable"), "{pg}");
        assert!(pg.contains("ok (2 ms)"), "{pg}");
        let s = sample(true, false).summary;
        // postgres: 18 applicable kinds, redis: 3 (database, key, user), x 3 checks.
        assert_eq!(s.pending, (18 + 3) * 3);
        assert_eq!(s.total, 2 * ObjectKind::ALL.len() * Check::ALL.len());
        assert_eq!(s.pending + s.not_applicable, s.total);
        assert!(text.contains(&format!(
            "Summary: {} cells: 0 pass, 0 fail, {} pending, 0 skipped, {} n/a",
            s.total, s.pending, s.not_applicable
        )));
        assert!(text.contains("no engine has a driver yet"));
    }

    #[test]
    fn table_columns_align() {
        let mut out = String::new();
        write_table(
            &mut out,
            &["A", "BB"],
            &[vec!["long-cell", "x"], vec!["y", ""]],
        );
        assert_eq!(out, "A          BB\nlong-cell  x\ny\n");
    }

    #[test]
    fn gate_fails_on_unreachable_naming_engine_and_port() {
        let report = sample(false, false);
        let err = report.gate().unwrap_err();
        assert!(err.contains("redis (redis) at 127.0.0.1:15479"), "{err}");
        assert!(err.contains("OXBASE_CONFORMANCE_ALLOW_MISSING=1"), "{err}");
        assert!(!err.contains("postgres"), "{err}");
        assert!(report.render().contains("UNREACHABLE"));
    }

    #[test]
    fn gate_passes_when_missing_allowed_or_all_up() {
        assert!(sample(false, true).gate().is_ok());
        assert!(sample(true, false).gate().is_ok());
        assert!(
            sample(false, true)
                .render()
                .contains("UNREACHABLE (allowed)")
        );
    }

    #[test]
    fn gate_always_fails_on_engine_without_fixture() {
        let mut report = sample(true, true);
        report.missing_engines = vec![Engine::MsSql];
        let err = report.gate().unwrap_err();
        assert!(err.contains("no fixture for: mssql"), "{err}");
    }

    #[test]
    fn json_round_trips() {
        let report = sample(true, false);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["milestone"], "M0");
        assert_eq!(json["engines"][0]["target"]["port"], 15432);
        let cell = &json["cells"][0];
        assert_eq!(cell["engine"], "postgres");
        assert_eq!(cell["object_kind"], "database");
        assert_eq!(cell["check"], "roundtrip");
        assert_eq!(cell["status"], "pending");
        assert_eq!(cell["detail"], "no driver yet");
        let back: Report = serde_json::from_value(json).unwrap();
        assert_eq!(back.summary, report.summary);
        assert_eq!(back.cells, report.cells);
    }

    #[test]
    fn report_path_is_under_target_conformance() {
        let p = default_report_path();
        assert!(
            p.ends_with(Path::new("conformance").join("report.json")),
            "{}",
            p.display()
        );
    }

    #[test]
    fn renders_engines_whose_version_was_not_checked() {
        let mut report = sample(true, false);
        assert!(!report.render().contains("Version not checked"));
        report.engines[0].probe.expected_version = Some("PostgreSQL 18".to_owned());
        let text = report.render();
        assert!(
            line_starting(&text, "postgres").contains("[version not checked]"),
            "{text}"
        );
        assert!(
            line_starting(&text, "Version not checked for: postgres ").contains("not verified"),
            "{text}"
        );
    }

    #[test]
    fn relative_target_dir_resolves_against_current_dir() {
        let cwd = std::env::temp_dir().join("cwd");
        assert_eq!(
            target_dir(Some("tgt".into()), Some(cwd.clone())),
            cwd.join("tgt")
        );
        let abs = std::env::temp_dir().join("abs-target");
        assert_eq!(
            target_dir(Some(abs.clone().into_os_string()), Some(cwd)),
            abs
        );
        assert_eq!(
            target_dir(Some(std::ffi::OsString::new()), None),
            in_workspace("target")
        );
        assert_eq!(target_dir(None, None), in_workspace("target"));
    }

    #[test]
    fn write_json_replaces_the_file_and_leaves_no_temp_file() {
        let dir =
            std::env::temp_dir().join(format!("oxbase-conformance-report-{}", std::process::id()));
        let path = dir.join("nested").join("r.json");
        let report = sample(true, false);
        report.write_json(&path).unwrap();
        report.write_json(&path).unwrap();
        let back: Report = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.summary, report.summary);
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("r.json")]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
