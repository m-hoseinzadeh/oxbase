//! Loading and validating `tests/fixtures.toml`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::adapter::{Engine, ObjectKind, engine_from_fixture_kind};
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
    /// M3: round-trip scratch names and the editable seed tables (`[m3]` and `[m3.<engine>]`).
    #[serde(default)]
    pub m3: Option<M3Fixture>,
    /// M4: SQL Server seed names (`[m4]` and `[m4.<engine>]`).
    #[serde(default)]
    pub m4: Option<M4Fixture>,
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

/// One `[m2.<engine>]` table. Unknown keys are rejected ([`Fixtures::parse`] names them), so a
/// misspelt key cannot silently empty a seed and leave its cells pending.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
    /// Every seed object name `[m2]` and `seed` define: what `objects` may list.
    pub fn seed_names<'a>(&'a self, seed: &'a M2EngineSeed) -> Vec<&'a str> {
        let shared = [
            &self.view,
            &self.trigger,
            &self.function,
            &self.procedure,
            &self.sequence,
            &self.check_parent,
            &self.check_child,
            &self.foreign_key,
            &self.index,
        ];
        let own = [
            &seed.matview,
            &seed.trigger_function,
            &seed.other_table,
            &seed.attached_table,
        ];
        self.tables
            .iter()
            .map(String::as_str)
            .chain(shared.into_iter().chain(own).filter_map(Option::as_deref))
            .collect()
    }

    /// The `[m2.<name>]` table, if there is one.
    pub fn engine(&self, name: &str) -> Option<&M2EngineSeed> {
        match self.engines.get(name)? {
            M2Entry::Engine(e) => Some(&**e),
            M2Entry::Other(_) => None,
        }
    }
}

/// The `[m3]` table (shape documented next to it in `tests/fixtures.toml`). Every field is
/// optional so a partial table never breaks the earlier checks; keys the harness does not know
/// are kept in `other` (the `[m3.<engine>]` tables among them), so another stream adding a key
/// cannot break the run.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct M3Fixture {
    /// Start of every round-trip scratch table name (`m3_rt_`).
    pub roundtrip_prefix: Option<String>,
    /// The kinds whose `roundtrip` cells run (`table`, `column`, `index`, `foreign_key`, `check`).
    pub roundtrip_kinds: Vec<String>,
    /// Scratch tables whose token is older than this are dropped when a run starts.
    pub stale_after_secs: Option<u64>,
    pub edit_pk_table: Option<String>,
    pub edit_unique_table: Option<String>,
    pub edit_nokey_table: Option<String>,
    pub edit_view: Option<String>,
    pub edit_seed_rows: Option<u64>,
    /// `[m3.<engine>]` tables and any other key, as written.
    #[serde(flatten)]
    pub other: BTreeMap<String, toml::Value>,
}

/// One `[m3.<engine>]` table: where the engine's M3 objects live.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct M3EngineSeed {
    /// The engine's M3 init script (seed tables), relative to the workspace root. The round trip
    /// does not need it: it creates its own scratch tables.
    pub init_sql: Option<String>,
    /// The database the scratch tables live in (`main` on SQLite).
    pub database: Option<String>,
    /// PostgreSQL: the schema the scratch tables live in.
    pub schema: Option<String>,
}

impl M3Fixture {
    /// The `[m3.<name>]` table, if there is one and it parses.
    pub fn engine(&self, name: &str) -> Option<M3EngineSeed> {
        match self.other.get(name)? {
            toml::Value::Table(t) => t.clone().try_into().ok(),
            _ => None,
        }
    }

    /// `roundtrip_kinds` as object kinds, in the order written. `Err` names an entry that is not
    /// an object kind.
    pub fn kinds(&self) -> Result<Vec<ObjectKind>, String> {
        self.roundtrip_kinds
            .iter()
            .map(|k| {
                k.parse::<ObjectKind>()
                    .map_err(|_| format!("m3.roundtrip_kinds: {k:?} is not an object kind"))
            })
            .collect()
    }
}

/// The `[m4]` table: names of the SQL Server objects of `docs/m4-contracts.md` §5.3 (shape
/// documented next to the table in `tests/fixtures.toml`), all in `schema`. Every field is
/// optional and unknown keys are kept in `other` (the `[m4.<engine>]` tables among them), like
/// [`M3Fixture`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct M4Fixture {
    /// The schema every `[m4]` object lives in.
    pub schema: Option<String>,
    pub types_table: Option<String>,
    pub features_table: Option<String>,
    pub temporal_table: Option<String>,
    pub temporal_history: Option<String>,
    pub indexed_view: Option<String>,
    /// The unique clustered index of `indexed_view` (listed under the view).
    pub indexed_view_index: Option<String>,
    /// The INSTEAD OF trigger of `indexed_view` (listed under the view).
    pub view_trigger: Option<String>,
    pub inline_tvf: Option<String>,
    pub multi_tvf: Option<String>,
    pub alias_type: Option<String>,
    pub table_type: Option<String>,
    pub tvp_procedure: Option<String>,
    pub messages_procedure: Option<String>,
    /// A database-level DDL trigger: not a table's trigger, never listed under one.
    pub ddl_trigger: Option<String>,
    /// The audit table of the `[m3]` edit table (schema of `[m3.<engine>]`).
    pub edit_audit_table: Option<String>,
    /// `[m4.<engine>]` tables and any other key, as written.
    #[serde(flatten)]
    pub other: BTreeMap<String, toml::Value>,
}

/// One `[m4.<engine>]` table: where the engine's M4 objects live.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct M4EngineSeed {
    /// The engine's M4 init script, relative to the workspace root (applied by the Docker init).
    pub init_sql: Option<String>,
    /// The database the `[m4]` schema lives in.
    pub database: Option<String>,
    /// The name a `VerifyFull` direct connection checks the certificate against.
    pub tls_server_name: Option<String>,
}

impl M4Fixture {
    /// The `[m4.<name>]` table, if there is one and it parses.
    pub fn engine(&self, name: &str) -> Option<M4EngineSeed> {
        match self.other.get(name)? {
            toml::Value::Table(t) => t.clone().try_into().ok(),
            _ => None,
        }
    }

    /// Every schema-level object `[m4]` names (tables, views, routines, types), without the
    /// view children and the DDL trigger.
    pub fn schema_objects(&self) -> Vec<&str> {
        [
            &self.types_table,
            &self.features_table,
            &self.temporal_table,
            &self.temporal_history,
            &self.indexed_view,
            &self.inline_tvf,
            &self.multi_tvf,
            &self.alias_type,
            &self.table_type,
            &self.tvp_procedure,
            &self.messages_procedure,
        ]
        .into_iter()
        .filter_map(Option::as_deref)
        .collect()
    }
}

impl Fixtures {
    /// The M4 table of the fixture `name` (falling back to the engine kind's name), with the
    /// shared `[m4]` table.
    pub fn m4_seed(&self, name: &str, engine: Engine) -> Option<(&M4Fixture, M4EngineSeed)> {
        let m4 = self.m4.as_ref()?;
        let seed = m4.engine(name).or_else(|| m4.engine(engine.as_str()))?;
        Some((m4, seed))
    }

    /// The M3 table of the fixture `name` (falling back to the engine kind's name), with the
    /// shared `[m3]` table.
    pub fn m3_seed(&self, name: &str, engine: Engine) -> Option<(&M3Fixture, M3EngineSeed)> {
        let m3 = self.m3.as_ref()?;
        let seed = m3.engine(name).or_else(|| m3.engine(engine.as_str()))?;
        Some((m3, seed))
    }

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
        fixtures.validate_m2()?;
        fixtures.validate_m3()?;
        fixtures.validate_m4()?;
        Ok(fixtures)
    }

    /// Rejects an `[m2]` the M2 checks would misread instead of failing: an unknown key, an
    /// `[m2.<name>]` table that does not parse (unknown key, wrong type), or an `objects` entry
    /// that names no seed object.
    fn validate_m2(&self) -> Result<(), FixturesError> {
        let Some(m2) = &self.m2 else {
            return Ok(());
        };
        for (name, entry) in &m2.engines {
            match entry {
                M2Entry::Engine(seed) => {
                    let known = m2.seed_names(seed);
                    if let Some(bad) = seed.objects.iter().find(|o| !known.contains(&o.as_str())) {
                        return Err(self.invalid(format!(
                            "m2.{name}.objects: {bad:?} is not a seed object named in [m2] or \
                             [m2.{name}]"
                        )));
                    }
                }
                M2Entry::Other(toml::Value::Table(t)) => {
                    let why = match t.clone().try_into::<M2EngineSeed>() {
                        Ok(_) => "not a seed table".to_owned(),
                        Err(e) => e.to_string(),
                    };
                    return Err(self.invalid(format!("m2.{name}: {}", why.trim())));
                }
                M2Entry::Other(_) => {
                    return Err(self.invalid(format!("m2: unknown key `{name}`")));
                }
            }
        }
        Ok(())
    }

    /// Rejects an `[m3]` the round trip would misread: a `roundtrip_kinds` entry that is not an
    /// object kind, or an `[m3.<name>]` table whose known keys have the wrong type.
    fn validate_m3(&self) -> Result<(), FixturesError> {
        let Some(m3) = &self.m3 else {
            return Ok(());
        };
        m3.kinds().map_err(|e| self.invalid(e))?;
        for (name, value) in &m3.other {
            if let toml::Value::Table(t) = value
                && let Err(e) = t.clone().try_into::<M3EngineSeed>()
            {
                return Err(self.invalid(format!("m3.{name}: {}", e.to_string().trim())));
            }
        }
        Ok(())
    }

    /// Rejects an `[m4.<name>]` table whose known keys have the wrong type.
    fn validate_m4(&self) -> Result<(), FixturesError> {
        let Some(m4) = &self.m4 else {
            return Ok(());
        };
        for (name, value) in &m4.other {
            if let toml::Value::Table(t) = value
                && let Err(e) = t.clone().try_into::<M4EngineSeed>()
            {
                return Err(self.invalid(format!("m4.{name}: {}", e.to_string().trim())));
            }
        }
        Ok(())
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
    fn rejects_m2_tables_the_checks_would_misread() {
        let base = "schema_version = 1\n[m2]\ntables = [\"a\", \"b\"]\nview = \"v\"\n";
        let ok = format!("{base}[m2.lite]\ndatabase = \"main\"\nobjects = [\"a\", \"v\"]\n");
        let fx = Fixtures::parse(&ok, "t").unwrap();
        let seed = fx.m2_seed("lite", Engine::Sqlite).unwrap().1;
        assert_eq!(seed.objects, ["a", "v"]);
        let cases: [(String, &[&str]); 4] = [
            (
                format!("{base}[m2.lite]\ndatabase = \"main\"\nobject = [\"a\"]\n"),
                &["m2.lite: ", "unknown field `object`"],
            ),
            (
                format!("{base}[m2.lite]\ndatabase = 1\n"),
                &["m2.lite: ", "invalid type"],
            ),
            (
                format!("{base}[m2.lite]\nobjects = [\"a\", \"veiw\"]\n"),
                &["m2.lite.objects: \"veiw\" is not a seed object"],
            ),
            (format!("{base}veiw = \"v\"\n"), &["m2: unknown key `veiw`"]),
        ];
        for (text, wants) in cases {
            let err = Fixtures::parse(&text, "t").unwrap_err().to_string();
            for want in wants {
                assert!(err.contains(want), "{text:?}: got {err:?}, want {want:?}");
            }
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
        // The M2 engines all seed objects (m2-contracts §8).
        for name in ["postgres", "mysql", "mariadb", "sqlite"] {
            let e = engines.iter().find(|e| e.name == name).unwrap();
            let (_, seed) = fx
                .m2_seed(name, e.engine)
                .unwrap_or_else(|| panic!("[m2.{name}]"));
            assert!(seed.init_sql.is_some(), "[m2.{name}] init_sql");
            assert!(seed.database.is_some(), "[m2.{name}] database");
            assert!(!seed.objects.is_empty(), "[m2.{name}] objects");
        }
        // The M3 round trip runs on the same four engines (m3-contracts §8).
        let m3 = fx.m3.as_ref().expect("[m3]");
        assert_eq!(
            m3.kinds().unwrap(),
            oxbase_dialect::DESIGNER_KINDS.to_vec(),
            "[m3] roundtrip_kinds"
        );
        assert!(m3.roundtrip_prefix.is_some(), "[m3] roundtrip_prefix");
        for name in ["postgres", "mysql", "mariadb", "sqlite"] {
            let e = engines.iter().find(|e| e.name == name).unwrap();
            let (_, seed) = fx
                .m3_seed(name, e.engine)
                .unwrap_or_else(|| panic!("[m3.{name}]"));
            assert!(seed.database.is_some(), "[m3.{name}] database");
        }
        // M4: SQL Server has the TLS user, the bastion target and all three seed tables
        // (m4-contracts §5, §8).
        assert!(
            ssh.targets.contains_key("mssql"),
            "[ssh.targets] lacks mssql"
        );
        assert!(
            fx.engines["mssql"].tls_user.is_some(),
            "mssql has no tls_user"
        );
        // [tls] server_sans and the certificate script's DNS names agree, mssql included.
        let sans: Vec<&str> = fx.tls.as_ref().unwrap().extra["server_sans"]
            .as_array()
            .expect("[tls] server_sans")
            .iter()
            .filter_map(toml::Value::as_str)
            .collect();
        let certs =
            std::fs::read_to_string(workspace_root().join("scripts/fixtures-certs.sh")).unwrap();
        let dns_line = certs
            .lines()
            .find_map(|l| l.trim().strip_prefix("server_dns_sans=("))
            .and_then(|l| l.strip_suffix(')'))
            .expect("server_dns_sans=(...) in scripts/fixtures-certs.sh");
        let dns: Vec<&str> = dns_line.split_whitespace().collect();
        assert!(sans.contains(&"mssql"), "[tls] server_sans: {sans:?}");
        for san in sans
            .iter()
            .filter(|s| s.parse::<std::net::IpAddr>().is_err())
        {
            assert!(
                dns.contains(san),
                "fixtures-certs.sh lacks DNS SAN {san}: {dns:?}"
            );
        }
        let (_, m2) = fx.m2_seed("mssql", Engine::MsSql).expect("[m2.mssql]");
        assert!(m2.init_sql.is_some() && m2.schema.is_some(), "[m2.mssql]");
        assert_eq!(m2.database.as_deref(), Some("oxbase_test"));
        assert!(m2.other_database.is_some(), "[m2.mssql] other_database");
        let (_, m3) = fx.m3_seed("mssql", Engine::MsSql).expect("[m3.mssql]");
        assert_eq!(m3.schema.as_deref(), Some("m3"));
        let (m4, seed) = fx.m4_seed("mssql", Engine::MsSql).expect("[m4.mssql]");
        assert_eq!(seed.database.as_deref(), Some("oxbase_test"));
        assert_eq!(seed.tls_server_name.as_deref(), Some("localhost"));
        assert_eq!(m4.schema.as_deref(), Some("m4"));
        assert_eq!(m4.schema_objects().len(), 11, "{:?}", m4.schema_objects());
        for (what, v) in [
            ("indexed_view_index", &m4.indexed_view_index),
            ("view_trigger", &m4.view_trigger),
            ("ddl_trigger", &m4.ddl_trigger),
            ("edit_audit_table", &m4.edit_audit_table),
        ] {
            assert!(v.is_some(), "[m4] {what}");
        }
        // Every init script the fixtures name exists, and the SQL Server ones are ASCII (sqlcmd
        // reads them in the container's code page).
        let mut scripts: Vec<String> = Vec::new();
        for e in &engines {
            if let Some((_, s)) = fx.m2_seed(&e.name, e.engine) {
                scripts.extend(s.init_sql.clone());
            }
            if let Some((_, s)) = fx.m3_seed(&e.name, e.engine) {
                scripts.extend(s.init_sql);
            }
            if let Some((_, s)) = fx.m4_seed(&e.name, e.engine) {
                scripts.extend(s.init_sql);
            }
        }
        assert!(scripts.len() >= 11, "{scripts:?}");
        for script in &scripts {
            let text = std::fs::read_to_string(in_workspace(script))
                .unwrap_or_else(|e| panic!("{script}: {e}"));
            if script.contains("/mssql/") {
                assert!(text.is_ascii(), "{script} is not ASCII");
            }
        }
        let dbg = format!("{ssh:?} {:?}", fx.engines["postgres"]);
        assert!(!dbg.contains(&ssh.password), "{dbg}");
        for e in &engines {
            if let Target::Tcp { port, .. } = e.target {
                assert!(port > 1024, "{} uses privileged port {port}", e.name);
            }
        }
    }

    const M3: &str = r#"
schema_version = 1
[engines.lite]
kind = "sqlite"
path = "x.sqlite"
[m3]
roundtrip_prefix = "m3_rt_"
roundtrip_kinds = ["table", "foreign_key"]
stale_after_secs = 60
edit_pk_table = "m3_edit_pk"
some_future_key = "kept"
[m3.sqlite]
init_sql = "tests/x.sql"
database = "main"
also_future = 1
"#;

    #[test]
    fn m3_table_parses_and_tolerates_new_keys() {
        let fx = Fixtures::parse(M3, "m3").unwrap();
        let m3 = fx.m3.as_ref().unwrap();
        assert_eq!(m3.roundtrip_prefix.as_deref(), Some("m3_rt_"));
        assert_eq!(m3.stale_after_secs, Some(60));
        assert_eq!(
            m3.kinds().unwrap(),
            vec![ObjectKind::Table, ObjectKind::ForeignKey]
        );
        // Found by the engine kind's name when no table has the fixture's name.
        let (_, seed) = fx.m3_seed("lite", Engine::Sqlite).unwrap();
        assert_eq!(seed.database.as_deref(), Some("main"));
        assert_eq!(seed.schema, None);
        assert!(fx.m3_seed("lite", Engine::Postgres).is_none());
        // No [m3] at all is fine.
        assert!(Fixtures::parse(SAMPLE, "s").unwrap().m3.is_none());
    }

    #[test]
    fn m4_table_parses_and_rejects_bad_types() {
        let text = format!(
            "{M3}[m4]\nschema = \"m4\"\nalias_type = \"t\"\nfuture = 1\n[m4.sqlite]\n\
             init_sql = \"x.sql\"\ndatabase = \"m4db\"\n"
        );
        let fx = Fixtures::parse(&text, "m4").unwrap();
        let (m4, seed) = fx.m4_seed("lite", Engine::Sqlite).unwrap();
        assert_eq!(m4.schema.as_deref(), Some("m4"));
        assert_eq!(m4.schema_objects(), ["t"]);
        assert_eq!(seed.database.as_deref(), Some("m4db"));
        assert!(fx.m4_seed("lite", Engine::MsSql).is_none());
        assert!(Fixtures::parse(M3, "m3").unwrap().m4.is_none());
        let bad = text.replace("database = \"m4db\"", "database = 3");
        let err = Fixtures::parse(&bad, "m4").unwrap_err().to_string();
        assert!(err.contains("m4.sqlite"), "{err}");
    }

    #[test]
    fn m3_table_rejects_bad_kinds_and_types() {
        let bad_kind = M3.replace("\"foreign_key\"]", "\"foreign_keys\"]");
        let err = Fixtures::parse(&bad_kind, "m3").unwrap_err().to_string();
        assert!(
            err.contains("\"foreign_keys\" is not an object kind"),
            "{err}"
        );
        let bad_type = M3.replace("database = \"main\"", "database = 3");
        let err = Fixtures::parse(&bad_type, "m3").unwrap_err().to_string();
        assert!(err.contains("m3.sqlite"), "{err}");
    }
}
