//! Loading and validating `tests/fixtures.toml`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::adapter::{Engine, engine_from_fixture_kind};
use crate::probe::Target;

/// The `schema_version` this harness understands.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

/// Host used when neither the engine nor `[defaults]` sets one.
pub const FALLBACK_HOST: &str = "127.0.0.1";

#[derive(Debug, thiserror::Error)]
pub enum FixturesError {
    #[error("cannot read fixtures file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse fixtures file {origin}: {source}")]
    Parse {
        origin: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid fixtures file {origin}: {message}")]
    Invalid { origin: String, message: String },
}

/// The workspace root, derived from this crate's manifest dir (`crates/oxbase-conformance`).
pub fn workspace_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| manifest.to_path_buf(), Path::to_path_buf)
}

/// Resolves `path` against the workspace root unless it is already absolute.
pub fn in_workspace(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_root().join(path)
    }
}

/// The fixtures file to use: `$OXBASE_FIXTURES` if set, else `<workspace>/tests/fixtures.toml`.
pub fn fixtures_path() -> PathBuf {
    resolve_fixtures_path(std::env::var_os("OXBASE_FIXTURES"))
}

/// [`fixtures_path`] with the override passed in, so it can be tested without touching the
/// process environment. An empty override counts as unset.
pub fn resolve_fixtures_path(override_path: Option<OsString>) -> PathBuf {
    match override_path.filter(|p| !p.is_empty()) {
        Some(p) => in_workspace(PathBuf::from(p)),
        None => workspace_root().join("tests").join("fixtures.toml"),
    }
}

/// The parsed fixtures file, as written.
#[derive(Debug, Clone, Deserialize)]
pub struct Fixtures {
    pub schema_version: u32,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub engines: BTreeMap<String, EngineFixture>,
    /// Where the file came from, for error messages. Not part of the TOML.
    #[serde(skip)]
    pub origin: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Defaults {
    pub host: Option<String>,
    pub compose_project: Option<String>,
    pub startup_timeout_secs: Option<u64>,
}

/// One `[engines.<name>]` table. Fields the harness does not use yet are kept in `extra`.
#[derive(Clone, Deserialize)]
pub struct EngineFixture {
    pub kind: String,
    pub image: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub container_port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    /// File path for file-based engines, relative to the workspace root.
    pub path: Option<String>,
    pub version_query: Option<String>,
    pub version_prefix: Option<String>,
    #[serde(flatten)]
    pub extra: toml::Table,
}

// Hand-written so the password never ends up in a panic message or log line.
impl std::fmt::Debug for EngineFixture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineFixture")
            .field("kind", &self.kind)
            .field("image", &self.image)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("database", &self.database)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// An engine entry after validation: what the harness actually probes and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEngine {
    /// The `[engines.<name>]` key.
    pub name: String,
    pub engine: Engine,
    pub target: Target,
    pub version_prefix: Option<String>,
}

impl Fixtures {
    /// Reads and parses the file at `path`.
    pub fn from_path(path: &Path) -> Result<Self, FixturesError> {
        let text = std::fs::read_to_string(path).map_err(|source| FixturesError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text, &path.display().to_string())
    }

    /// Parses fixtures TOML. `origin` only labels error messages.
    pub fn parse(text: &str, origin: &str) -> Result<Self, FixturesError> {
        let mut fixtures: Self = toml::from_str(text).map_err(|source| FixturesError::Parse {
            origin: origin.to_owned(),
            source,
        })?;
        origin.clone_into(&mut fixtures.origin);
        if fixtures.schema_version != SUPPORTED_SCHEMA_VERSION {
            return Err(fixtures.invalid(format!(
                "schema_version {} is not supported (expected {SUPPORTED_SCHEMA_VERSION})",
                fixtures.schema_version
            )));
        }
        Ok(fixtures)
    }

    /// The compose project the fixture containers run under, if the file names one.
    pub fn compose_project(&self) -> Option<&str> {
        self.defaults.compose_project.as_deref()
    }

    /// Validates every engine entry and returns them in report order (engine, then name).
    pub fn resolve(&self) -> Result<Vec<ResolvedEngine>, FixturesError> {
        let mut out = Vec::with_capacity(self.engines.len());
        for (name, fx) in &self.engines {
            let engine = engine_from_fixture_kind(&fx.kind).ok_or_else(|| {
                let known: Vec<_> = Engine::ALL.iter().map(|e| e.as_str()).collect();
                self.invalid(format!(
                    "engines.{name}: unknown kind {:?} (known: {})",
                    fx.kind,
                    known.join(", ")
                ))
            })?;
            let target = if engine.is_networked() {
                let port = fx
                    .port
                    .filter(|&p| p != 0)
                    .ok_or_else(|| self.invalid(format!("engines.{name}: missing `port`")))?;
                let host = fx
                    .host
                    .clone()
                    .or_else(|| self.defaults.host.clone())
                    .unwrap_or_else(|| FALLBACK_HOST.to_owned());
                Target::Tcp { host, port }
            } else {
                let path = fx
                    .path
                    .as_deref()
                    .filter(|p| !p.trim().is_empty())
                    .ok_or_else(|| self.invalid(format!("engines.{name}: missing `path`")))?;
                Target::File {
                    path: in_workspace(path),
                }
            };
            out.push(ResolvedEngine {
                name: name.clone(),
                engine,
                target,
                version_prefix: fx.version_prefix.clone(),
            });
        }
        out.sort_by(|a, b| (a.engine, &a.name).cmp(&(b.engine, &b.name)));
        Ok(out)
    }

    fn invalid(&self, message: String) -> FixturesError {
        FixturesError::Invalid {
            origin: self.origin.clone(),
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const SAMPLE: &str = r#"
schema_version = 1

[defaults]
host = "10.0.0.5"
compose_project = "oxbase-fixtures"
startup_timeout_secs = 90

[engines.pg]
kind = "postgres"
port = 15432
user = "oxbase"
password = "secret-pw"
version_prefix = "PostgreSQL 18"

[engines.maria]
kind = "MariaDB"
host = "127.0.0.1"
port = 15407
root_password = "x"

[engines.lite]
kind = "sqlite"
path = "target/fixtures/t.sqlite"
"#;

    #[test]
    fn parses_sample_and_resolves_in_engine_order() {
        let fx = Fixtures::parse(SAMPLE, "sample").unwrap();
        assert_eq!(fx.compose_project(), Some("oxbase-fixtures"));
        assert_eq!(fx.defaults.startup_timeout_secs, Some(90));
        // Unknown keys are kept, not rejected.
        assert!(fx.engines["maria"].extra.contains_key("root_password"));

        let engines = fx.resolve().unwrap();
        let summary: Vec<_> = engines
            .iter()
            .map(|e| (e.name.as_str(), e.engine, e.target.to_string()))
            .collect();
        let lite_path = workspace_root().join("target/fixtures/t.sqlite");
        assert_eq!(
            summary,
            vec![
                ("pg", Engine::Postgres, "10.0.0.5:15432".to_owned()),
                ("maria", Engine::MariaDb, "127.0.0.1:15407".to_owned()),
                ("lite", Engine::Sqlite, lite_path.display().to_string()),
            ]
        );
        assert_eq!(engines[0].version_prefix.as_deref(), Some("PostgreSQL 18"));
    }

    #[test]
    fn debug_output_redacts_password() {
        let fx = Fixtures::parse(SAMPLE, "sample").unwrap();
        let dbg = format!("{:?}", fx.engines["pg"]);
        assert!(!dbg.contains("secret-pw"), "{dbg}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
    }

    #[test]
    fn host_falls_back_to_loopback() {
        let fx = Fixtures::parse(
            "schema_version = 1\n[engines.r]\nkind = \"redis\"\nport = 1\n",
            "t",
        )
        .unwrap();
        assert_eq!(fx.resolve().unwrap()[0].target.to_string(), "127.0.0.1:1");
    }

    #[test]
    fn rejects_bad_files_with_clear_messages() {
        let cases = [
            ("schema_version = 2\n", "schema_version 2 is not supported"),
            (
                "schema_version = 1\n[engines.x]\nkind = \"oracle\"\nport = 1\n",
                "engines.x: unknown kind \"oracle\"",
            ),
            (
                "schema_version = 1\n[engines.pg]\nkind = \"postgres\"\n",
                "engines.pg: missing `port`",
            ),
            (
                "schema_version = 1\n[engines.l]\nkind = \"sqlite\"\n",
                "engines.l: missing `path`",
            ),
            (
                "schema_version = 1\n[engines.pg]\nkind = \"postgres\"\nport = 70000\n",
                "cannot parse",
            ),
            ("schema_version = ", "cannot parse fixtures file t"),
        ];
        for (text, want) in cases {
            let err = Fixtures::parse(text, "t")
                .and_then(|f| f.resolve().map(|_| ()))
                .expect_err(text)
                .to_string();
            assert!(err.contains(want), "{text:?}: got {err:?}, want {want:?}");
        }
    }

    #[test]
    fn missing_file_names_the_path() {
        let err = Fixtures::from_path(Path::new("/nonexistent/oxbase/fixtures.toml")).unwrap_err();
        assert!(
            err.to_string()
                .contains("/nonexistent/oxbase/fixtures.toml")
        );
    }

    #[test]
    fn fixtures_path_resolution() {
        let root = workspace_root();
        assert!(root.join("Cargo.toml").is_file(), "{}", root.display());
        assert_eq!(
            resolve_fixtures_path(None),
            root.join("tests").join("fixtures.toml")
        );
        assert_eq!(
            resolve_fixtures_path(Some(OsString::new())),
            root.join("tests").join("fixtures.toml")
        );
        assert_eq!(
            resolve_fixtures_path(Some("other/f.toml".into())),
            root.join("other/f.toml")
        );
        // Built from temp_dir so it is absolute on every OS ("/abs" has no drive on Windows).
        let abs = std::env::temp_dir().join("f.toml");
        assert!(abs.is_absolute());
        assert_eq!(
            resolve_fixtures_path(Some(abs.clone().into_os_string())),
            abs
        );
    }

    /// The checked-in file must parse and cover every v1 engine at least once (an engine may
    /// have fixtures for several versions).
    #[test]
    fn repo_fixtures_file_covers_every_engine() {
        let path = workspace_root().join("tests").join("fixtures.toml");
        let engines = Fixtures::from_path(&path).unwrap().resolve().unwrap();
        let kinds: Vec<_> = engines.iter().map(|e| e.engine).collect();
        let missing: Vec<_> = Engine::ALL.iter().filter(|e| !kinds.contains(e)).collect();
        assert!(
            missing.is_empty(),
            "no fixture for {missing:?} in {}",
            path.display()
        );
        for e in &engines {
            if let Target::Tcp { port, .. } = e.target {
                assert!(port > 1024, "{} uses privileged port {port}", e.name);
            }
        }
    }
}
