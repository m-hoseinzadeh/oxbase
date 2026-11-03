//! Golden pair reports (`docs/m8-contracts.md` §4.1): for every ordered pair of the five SQL
//! engines, `pair_report` over `representative_types()` is pinned in
//! `tests/golden/<source>_<target>.txt` (`PairReport::to_text`).
//!
//! After an intended mapping change, regenerate the files with
//! `OXBASE_BLESS=1 cargo test -p oxbase-transfer --test golden` and review the diff. A mismatch
//! prints the whole new text of every file that differs.

use std::path::{Path, PathBuf};

use oxbase_dialect::{DialectRegistry, M8_ENGINES};
use oxbase_transfer::mapping::{MappingContext, pair_report};
use oxbase_types::{Engine, ServerVersion};

/// The fixture server versions (`tests/fixtures.toml`).
fn version(engine: Engine) -> ServerVersion {
    match engine {
        Engine::Postgres => ServerVersion::new(18, 6, 0),
        Engine::MySql => ServerVersion::new(8, 4, 13),
        Engine::MariaDb => ServerVersion::new(11, 8, 9),
        Engine::MsSql => ServerVersion::new(17, 0, 0),
        _ => ServerVersion::new(3, 50, 4),
    }
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

#[test]
fn pair_reports_match_the_golden_files() {
    let mut reg = DialectRegistry::new();
    oxbase_transfer::register(&mut reg);
    let bless = std::env::var("OXBASE_BLESS").is_ok_and(|v| v == "1");
    let mut mismatches = Vec::new();
    let mut files = 0;
    for source in M8_ENGINES {
        for target in M8_ENGINES {
            let (sv, tv) = (version(source), version(target));
            let source_mapper = reg.type_mapper(source).unwrap();
            let target_mapper = reg.type_mapper(target).unwrap();
            let ctx = MappingContext {
                source_engine: source,
                source_version: &sv,
                source_mapper: source_mapper.as_ref(),
                target_engine: target,
                target_version: &tv,
                target_mapper: target_mapper.as_ref(),
            };
            let report = pair_report(&ctx);
            assert_eq!(report.rows.len(), 30, "{source} -> {target}");
            for row in &report.rows {
                assert!(
                    row.mapping.fidelity == oxbase_dialect::Fidelity::Exact
                        || row.mapping.reason.is_some(),
                    "{source} -> {target}: {} has no reason",
                    row.logical
                );
            }
            let text = report.to_text();
            let path = golden_dir().join(format!("{}_{}.txt", source.as_str(), target.as_str()));
            files += 1;
            let found = std::fs::read_to_string(&path).unwrap_or_default();
            if found != text {
                if bless {
                    std::fs::create_dir_all(golden_dir()).unwrap();
                    std::fs::write(&path, &text).unwrap();
                } else {
                    mismatches.push(format!(
                        "==> {} <==\n{text}",
                        path.file_name().unwrap().to_string_lossy()
                    ));
                }
            }
        }
    }
    assert_eq!(files, 25);
    assert!(
        mismatches.is_empty(),
        "{} golden pair report(s) differ; new texts:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}
