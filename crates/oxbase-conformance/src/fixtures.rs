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
    /// M1: the test PKI (`[tls]`).
    #[serde(default)]
    pub tls: Option<TlsFixture>,
    /// M1: the SSH bastion (`[ssh]`).
    #[serde(default)]
    pub ssh: Option<SshFixture>,
    /// M2: the seed objects the fetchers must find (`[m2]` and `[m2.<engine>]`).
    #[serde(default)]
    pub m2: Option<M2Fixture>,
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
    /// M1: a user the server accepts only over TLS (`[engines.<name>.tls_user]`).
    #[serde(default)]
    pub tls_user: Option<Credentials>,
    #[serde(flatten)]
    pub extra: toml::Table,
}

/// A user, password and database (`[engines.<name>.tls_user]`).
#[derive(Clone, Deserialize)]
pub struct Credentials {
    pub user: String,
    pub password: String,
    pub database: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("database", &self.database)
            .finish()
    }
}

/// The `[tls]` table: the test PKI. File names are relative to `dir`, which is relative to the
/// workspace root.
#[derive(Debug, Clone, Deserialize)]
pub struct TlsFixture {
    pub dir: String,
    pub ca_cert: String,
    #[serde(flatten)]
    pub extra: toml::Table,
}

impl TlsFixture {
    /// Absolute path of the CA certificate.
    pub fn ca_cert_path(&self) -> PathBuf {
        in_workspace(&self.dir).join(&self.ca_cert)
    }
}

/// The `[ssh]` table: the OpenSSH bastion and its key material.
#[derive(Clone, Deserialize)]
pub struct SshFixture {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub key_dir: String,
    pub client_key: String,
    pub client_key_encrypted: String,
    pub client_key_passphrase: String,
    pub known_hosts: String,
    pub host_key_fingerprint: Option<String>,
    /// In-network address of each engine fixture, by fixture name.
    #[serde(default)]
    pub targets: BTreeMap<String, SshTarget>,
    #[serde(flatten)]
    pub extra: toml::Table,
}

impl SshFixture {
    /// Absolute path of a file in `key_dir`.
    pub fn key_file(&self, name: &str) -> PathBuf {
        in_workspace(&self.key_dir).join(name)
    }
}

impl std::fmt::Debug for SshFixture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshFixture")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("key_dir", &self.key_dir)
            .field("client_key_passphrase", &"<redacted>")
            .field("targets", &self.targets)
            .finish_non_exhaustive()
    }
}

/// Where the bastion reaches an engine (`[ssh.targets]`).
#[derive(Debug, Clone, Deserialize)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
}

/// The `[m2]` table: names of the objects every engine's M2 seed script creates (the shape is
/// documented next to the table in `tests/fixtures.toml`). Every field is optional so a partial
/// table never breaks the M0/M1 checks.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct M2Fixture {
    /// `[parent table, child table]`: the child references the parent.
    pub tables: Vec<String>,
    pub view: Option<String>,
    pub trigger: Option<String>,
    /// The table `trigger` fires on.
    pub trigger_table: Option<String>,
    pub function: Option<String>,
    pub procedure: Option<String>,
    pub sequence: Option<String>,
    /// Check constraint on the parent table.
    pub check_parent: Option<String>,
    /// Check constraint on the child table.
    pub check_child: Option<String>,
    /// Foreign key of the child table.
    pub foreign_key: Option<String>,
    /// Index on the child table.
    pub index: Option<String>,
    pub table_comment: Option<String>,
    pub large_result_rows: Option<u64>,
    /// `[m2.<engine>]` tables, by fixture name (other unknown keys are kept as values).
    #[serde(flatten)]
    pub engines: BTreeMap<String, M2Entry>,
}

/// One unknown key of `[m2]`: an engine table, or anything else.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum M2Entry {
    Engine(Box<M2EngineSeed>),
    Other(toml::Value),
}

/// One `[m2.<engine>]` table.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct M2EngineSeed {
    /// The engine's seed script, relative to the workspace root. Applied by the Docker init for
    /// server engines; the harness applies it itself for SQLite.
    pub init_sql: Option<String>,
    /// The database the seed objects live in (`main` on SQLite).
    pub database: Option<String>,
    /// PostgreSQL: the schema the seed objects live in.
    pub schema: Option<String>,
    pub matview: Option<String>,
    pub trigger_function: Option<String>,
    /// PostgreSQL: a second database the database listing must show.
    pub other_database: Option<String>,
    pub other_table: Option<String>,
    /// SQLite: the name a second file is attached under.
    pub attached_as: Option<String>,
    pub attached_table: Option<String>,
    /// The seed objects that exist on this engine (tables, views, routines, triggers, ...).
    pub objects: Vec<String>,
}

impl M2Fixture {
    /// The `[m2.<name>]` table, if there is one.
    pub fn engine(&self, name: &str) -> Option<&M2EngineSeed> {
        match self.engines.get(name)? {
            M2Entry::Engine(e) => Some(&**e),
            M2Entry::Other(_) => None,
        }
    }
}

impl Fixtures {
    /// The M2 seed of the fixture `name` (falling back to the engine kind's name), with the
    /// shared `[m2]` table.
    pub fn m2_seed(&self, name: &str, engine: Engine) -> Option<(&M2Fixture, &M2EngineSeed)> {
        let m2 = self.m2.as_ref()?;
        let seed = m2.engine(name).or_else(|| m2.engine(engine.as_str()))?;
        Some((m2, seed))
    }
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
            .field("tls_user", &self.tls_user)
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
        let fx = Fixtures::from_path(&path).unwrap();
        let ssh = fx.ssh.as_ref().expect("[ssh]");
        for name in ["postgres", "mysql", "mariadb"] {
            assert!(ssh.targets.contains_key(name), "[ssh.targets] lacks {name}");
            assert!(
                fx.engines[name].tls_user.is_some(),
                "{name} has no tls_user"
            );
        }
        assert!(fx.tls.is_some(), "[tls]");
        let dbg = format!("{ssh:?} {:?}", fx.engines["postgres"]);
        assert!(!dbg.contains(&ssh.password), "{dbg}");
        for e in &engines {
            if let Target::Tcp { port, .. } = e.target {
                assert!(port > 1024, "{} uses privileged port {port}", e.name);
            }
        }
    }
}
