//! [`FileProfileRepository`]: connection profiles in one JSON file in the app config directory.
//!
//! # File format
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "profiles": [ { "schema_version": 1, "id": "...", "name": "...", ... } ]
//! }
//! ```
//!
//! Each element of `profiles` is a [`ConnectionProfile`] document with its own
//! `schema_version`. Unknown fields are preserved at every level: at the file's top level (kept
//! verbatim), inside each profile (`extra`), and whole profiles this build cannot parse (kept
//! verbatim and skipped by `list`/`get`, never deleted by a `save`). A file written by a newer
//! build keeps its `schema_version`.
//!
//! The file never holds a secret value: profiles refer to secrets only by keychain key or as
//! "prompt" ([`crate::SecretRef`], [`crate::KeyRef`]). `save` also refuses, so that a value sent
//! by the UI or edited by hand cannot slip a secret in:
//! - `options` whose key names a secret (`password`, `token`, ...);
//! - unknown (`extra`) fields at any depth, at the profile's top level and inside `tls` and
//!   `ssh`, whose key names a secret and that hold a string value (an object is a reference and
//!   is allowed, so newer builds can add secret *references*);
//! - any string in those fields, and any inline certificate (`tls.ca_cert`, `tls.client_cert`),
//!   that holds a PEM private key.
//!
//! # Writes
//!
//! Every write is atomic: the new document goes to a temporary file in the same directory, is
//! flushed to disk, and is renamed over the old file, so a crash leaves either the old or the new
//! file, never a torn one. On Unix the directory is created `0700` and the file `0600`. A
//! repository serialises its own read-modify-write cycles; two processes writing the same file at
//! the same moment may lose one update (last rename wins), but never corrupt the file.
//!
//! A file that is not valid JSON is never overwritten: `list`/`get` and `save`/`delete` fail with
//! `InvalidInput` until the user fixes or removes it.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use oxbase_types::{Error, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ConnectionProfile, Extra, PemRef, ProfileRepository, sort_profiles};

/// Current `schema_version` of the profiles file (the container, not the profiles in it).
pub const PROFILES_FILE_SCHEMA_VERSION: u32 = 1;

/// Environment variable that overrides [`app_config_dir`] (portable installs, tests, CI).
pub const CONFIG_DIR_ENV: &str = "OXBASE_CONFIG_DIR";

/// The app's configuration directory:
///
/// | Platform | Directory |
/// |---|---|
/// | Linux / BSD | `$XDG_CONFIG_HOME/oxbase` or `~/.config/oxbase` |
/// | macOS | `~/Library/Application Support/dev.oxbase.app` |
/// | Windows | `%APPDATA%\oxbase` (roaming) |
///
/// A non-empty [`CONFIG_DIR_ENV`] (`OXBASE_CONFIG_DIR`) wins. The directory is not created
/// here.
pub fn app_config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV).filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let base = dirs::config_dir().ok_or_else(|| {
        Error::unsupported(format!(
            "cannot find the user configuration directory; set {CONFIG_DIR_ENV}"
        ))
    })?;
    Ok(base.join(app_dir_name()))
}

/// The per-platform name of the app's directory under the user config (and data) directory.
pub(crate) const fn app_dir_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "dev.oxbase.app"
    } else {
        "oxbase"
    }
}

/// The on-disk document. Profiles stay raw JSON so unparsable (future) ones survive.
#[derive(Debug, Serialize, Deserialize)]
struct ProfilesFile {
    #[serde(default = "file_schema_version")]
    schema_version: u32,
    #[serde(default)]
    profiles: Vec<serde_json::Value>,
    #[serde(flatten)]
    extra: Extra,
}

fn file_schema_version() -> u32 {
    PROFILES_FILE_SCHEMA_VERSION
}

impl Default for ProfilesFile {
    fn default() -> Self {
        Self {
            schema_version: PROFILES_FILE_SCHEMA_VERSION,
            profiles: Vec::new(),
            extra: Extra::new(),
        }
    }
}

impl ProfilesFile {
    /// The parsed profiles; elements this build cannot parse are skipped.
    fn parsed(&self) -> Vec<ConnectionProfile> {
        self.profiles
            .iter()
            .filter_map(|v| serde_json::from_value::<ConnectionProfile>(v.clone()).ok())
            .collect()
    }

    /// Index of the element whose `id` is `id` (by the raw `id` field, so unparsable elements
    /// with the same id are replaced or deleted too).
    fn position(&self, id: Uuid) -> Option<usize> {
        let id = id.to_string();
        self.profiles.iter().position(|v| {
            v.get("id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case(&id))
        })
    }
}

struct Inner {
    path: PathBuf,
    /// Serialises this repository's read-modify-write cycles.
    write_lock: Mutex<()>,
}

/// [`ProfileRepository`] persisted as one JSON file (see the module docs).
#[derive(Clone)]
pub struct FileProfileRepository {
    inner: Arc<Inner>,
}

impl FileProfileRepository {
    /// File name inside the config directory.
    pub const FILE_NAME: &'static str = "connections.json";

    /// A repository stored in the file `path` (created on first save).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                path: path.into(),
                write_lock: Mutex::new(()),
            }),
        }
    }

    /// A repository stored as [`Self::FILE_NAME`] in `dir`.
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        Self::new(dir.as_ref().join(Self::FILE_NAME))
    }

    /// The repository in the app config directory ([`app_config_dir`]).
    pub fn open_default() -> Result<Self> {
        Ok(Self::in_dir(app_config_dir()?))
    }

    /// The JSON file.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    async fn blocking<T, F>(&self, op: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Inner) -> Result<T> + Send + 'static,
    {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || op(&inner))
            .await
            .map_err(|_| Error::internal("profile file task failed"))?
    }
}

impl fmt::Debug for FileProfileRepository {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileProfileRepository")
            .field("path", &self.inner.path)
            .finish()
    }
}

impl Inner {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        self.write_lock
            .lock()
            .map_err(|_| Error::internal("profile file lock poisoned"))
    }

    fn read(&self) -> Result<ProfilesFile> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ProfilesFile::default()),
            Err(e) => return Err(io_error(&e, "read", &self.path)),
        };
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(ProfilesFile::default());
        }
        serde_json::from_slice(&bytes).map_err(|e| {
            Error::invalid_input(format!(
                "connection profiles file {} is not valid: {e}; fix or move it away",
                self.path.display()
            ))
        })
    }

    fn write(&self, doc: &ProfilesFile) -> Result<()> {
        let mut json = serde_json::to_vec_pretty(doc)
            .map_err(|e| Error::internal(format!("cannot serialize connection profiles: {e}")))?;
        json.push(b'\n');
        write_atomic(&self.path, &json)
    }
}

/// Writes `bytes` to `path` atomically: temp file in the same directory, fsync, rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    create_private_dir(&dir)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| Error::invalid_input(format!("{} is not a file path", path.display())))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".{}.tmp", Uuid::new_v4().simple()));
    let tmp = dir.join(tmp_name);

    let result = (|| {
        let mut file = create_private_file(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        sync_dir(&dir);
        Ok::<(), io::Error>(())
    })();
    result.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        io_error(&e, "write", path)
    })
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if dir.is_dir() {
        return Ok(());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| io_error(&e, "create", dir))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|e| io_error(&e, "create", dir))
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Makes the rename durable (best effort; not possible on Windows).
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

fn io_error(e: &io::Error, action: &str, path: &Path) -> Error {
    Error::Io {
        kind: format!("{:?}", e.kind()),
        message: format!("cannot {action} {}: {e}", path.display()),
    }
}

/// Option keys that name a secret; such values belong in the keychain, never in the file.
fn option_names_secret(key: &str) -> bool {
    const MARKERS: [&str; 7] = [
        "password",
        "passwd",
        "pwd",
        "secret",
        "token",
        "passphrase",
        "credential",
    ];
    let key = key.to_ascii_lowercase();
    MARKERS.iter().any(|m| key.contains(m))
}

/// A PEM private key block (`PRIVATE KEY`, `RSA PRIVATE KEY`, `OPENSSH PRIVATE KEY`, ...).
fn holds_private_key(text: &str) -> bool {
    text.contains("PRIVATE KEY-----")
}

/// A value that may be a secret: a non-empty string, or an array holding one.
fn holds_value(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(items) => items.iter().any(holds_value),
        _ => false,
    }
}

/// The path (`a.b.0.c`) of the first field in `map` that would put a secret into the file: a
/// secret-named key with a string value, or any string holding a PEM private key.
fn find_inline_secret(map: &Extra) -> Option<String> {
    map.iter().find_map(|(key, value)| {
        if option_names_secret(key) && holds_value(value) {
            return Some(key.clone());
        }
        nested_inline_secret(value).map(|inner| {
            if inner.is_empty() {
                key.clone()
            } else {
                format!("{key}.{inner}")
            }
        })
    })
}

/// Like [`find_inline_secret`] for one value; `Some("")` means the value itself.
fn nested_inline_secret(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if holds_private_key(s) => Some(String::new()),
        serde_json::Value::Object(map) => find_inline_secret(map),
        serde_json::Value::Array(items) => items.iter().enumerate().find_map(|(i, item)| {
            nested_inline_secret(item).map(|inner| {
                if inner.is_empty() {
                    i.to_string()
                } else {
                    format!("{i}.{inner}")
                }
            })
        }),
        _ => None,
    }
}

/// Refuses profiles that would put a secret into the file. Errors name the field, never the
/// value.
fn check_no_inline_secrets(profile: &ConnectionProfile) -> Result<()> {
    if let Some(key) = profile.options.keys().find(|k| option_names_secret(k)) {
        return Err(Error::invalid_input(format!(
            "connection option '{key}' looks like a secret; store secrets in the keychain, \
             not in connection options"
        )));
    }
    if let Some((field, _)) = profile
        .options
        .iter()
        .find(|(_, value)| holds_private_key(value))
    {
        return Err(Error::invalid_input(format!(
            "connection option '{field}' holds a private key; use a key file or the keychain"
        )));
    }
    let mut extras = vec![("", &profile.extra), ("tls.", &profile.tls.extra)];
    if let Some(ssh) = &profile.ssh {
        extras.push(("ssh.", &ssh.extra));
    }
    for (prefix, extra) in extras {
        if let Some(path) = find_inline_secret(extra) {
            return Err(Error::invalid_input(format!(
                "connection field '{prefix}{path}' looks like a secret value; store secrets in \
                 the keychain, not in the connection file"
            )));
        }
    }
    for (field, pem) in [
        ("tls.ca_cert", profile.tls.ca_cert.as_ref()),
        ("tls.client_cert", profile.tls.client_cert.as_ref()),
    ] {
        if let Some(PemRef::Inline { pem }) = pem
            && holds_private_key(pem)
        {
            return Err(Error::invalid_input(format!(
                "{field} holds a private key; certificates may be inline, private keys must be \
                 a key file or stored in the keychain"
            )));
        }
    }
    Ok(())
}

#[async_trait]
impl ProfileRepository for FileProfileRepository {
    async fn list(&self) -> Result<Vec<ConnectionProfile>> {
        self.blocking(|inner| {
            let mut all = inner.read()?.parsed();
            sort_profiles(&mut all);
            Ok(all)
        })
        .await
    }

    async fn get(&self, id: Uuid) -> Result<Option<ConnectionProfile>> {
        self.blocking(move |inner| Ok(inner.read()?.parsed().into_iter().find(|p| p.id == id)))
            .await
    }

    async fn save(&self, profile: &ConnectionProfile) -> Result<()> {
        profile.validate()?;
        check_no_inline_secrets(profile)?;
        let value = serde_json::to_value(profile)
            .map_err(|e| Error::internal(format!("cannot serialize connection profile: {e}")))?;
        let id = profile.id;
        self.blocking(move |inner| {
            let _guard = inner.lock()?;
            let mut doc = inner.read()?;
            match doc.position(id) {
                Some(i) => doc.profiles[i] = value,
                None => doc.profiles.push(value),
            }
            inner.write(&doc)
        })
        .await
    }

    async fn delete(&self, id: Uuid) -> Result<bool> {
        self.blocking(move |inner| {
            let _guard = inner.lock()?;
            let mut doc = inner.read()?;
            let Some(i) = doc.position(id) else {
                return Ok(false);
            };
            doc.profiles.remove(i);
            inner.write(&doc)?;
            Ok(true)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Engine, KeyRef, KnownHostsPolicy, MemorySecretStore, PemRef, ProfileAddress, Secret,
        SecretRef, SecretSlot, SecretStore, SshAuthSettings, SshSettings, TlsMode, TlsSettings,
    };
    use pretty_assertions::assert_eq;

    const MARKERS: [&str; 5] = [
        "MARKER-db-password-7f3a",
        "MARKER-ssh-password-91bc",
        "MARKER-passphrase-22de",
        "MARKER-ssh-key-body-5e10",
        "MARKER-tls-key-body-c4d9",
    ];

    fn profile(name: &str) -> ConnectionProfile {
        let mut p = ConnectionProfile::new(
            name,
            Engine::Postgres,
            ProfileAddress::Network {
                host: "db.internal".into(),
                port: Some(5432),
            },
        );
        p.user = Some("app".into());
        p.password = Some(SecretRef::Keychain {
            key: p.secret_key(SecretSlot::Password),
        });
        p
    }

    /// A profile that uses every secret slot, each in the keychain.
    fn profile_with_every_secret() -> ConnectionProfile {
        let mut p = profile("every secret");
        p.tls = TlsSettings {
            mode: TlsMode::VerifyFull,
            ca_cert: Some(PemRef::Inline {
                pem: "-----BEGIN CERTIFICATE-----\npublic\n-----END CERTIFICATE-----".into(),
            }),
            client_cert: Some(PemRef::File {
                path: "/etc/db/client.pem".into(),
            }),
            client_key: Some(KeyRef::Keychain {
                key: p.secret_key(SecretSlot::TlsClientKey),
            }),
            server_name: Some("db.example".into()),
            extra: Extra::new(),
        };
        p.ssh = Some(SshSettings {
            host: "bastion".into(),
            port: 2222,
            user: "ops".into(),
            auth: SshAuthSettings::PrivateKey {
                key: KeyRef::Keychain {
                    key: p.secret_key(SecretSlot::SshPrivateKey),
                },
                passphrase: Some(SecretRef::Keychain {
                    key: p.secret_key(SecretSlot::SshKeyPassphrase),
                }),
            },
            known_hosts: KnownHostsPolicy::AcceptNew { path: None },
            connect_timeout_secs: Some(10),
            keepalive_secs: Some(30),
            extra: Extra::new(),
        });
        p
    }

    fn repo() -> (tempfile::TempDir, FileProfileRepository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = FileProfileRepository::in_dir(dir.path().join("nested").join("oxbase"));
        (dir, repo)
    }

    #[tokio::test]
    async fn crud_round_trip_and_ordering() {
        let (_dir, repo) = repo();
        assert_eq!(repo.list().await.unwrap(), Vec::new());
        assert!(!repo.path().exists());

        let b = profile("beta");
        let a = profile("Alpha");
        repo.save(&b).await.unwrap();
        repo.save(&a).await.unwrap();
        let names: Vec<_> = repo
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["Alpha", "beta"]);
        assert_eq!(repo.get(a.id).await.unwrap(), Some(a.clone()));

        // Upsert by id.
        let mut a2 = a.clone();
        a2.name = "gamma".into();
        repo.save(&a2).await.unwrap();
        let all = repo.list().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1], a2);

        // A second repository on the same file sees the same data.
        let again = FileProfileRepository::new(repo.path());
        assert_eq!(again.list().await.unwrap(), all);

        assert!(repo.delete(a.id).await.unwrap());
        assert!(!repo.delete(a.id).await.unwrap());
        assert_eq!(repo.get(a.id).await.unwrap(), None);
        assert_eq!(repo.list().await.unwrap(), vec![b]);

        let mut invalid = profile(" ");
        invalid.name = " ".into();
        assert!(repo.save(&invalid).await.is_err());
    }

    #[tokio::test]
    async fn file_has_schema_version_and_no_temp_files_remain() {
        let (_dir, repo) = repo();
        repo.save(&profile("one")).await.unwrap();
        repo.save(&profile("two")).await.unwrap();
        let text = fs::read_to_string(repo.path()).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["schema_version"], PROFILES_FILE_SCHEMA_VERSION);
        assert_eq!(
            doc["profiles"][0]["schema_version"],
            crate::PROFILE_SCHEMA_VERSION
        );
        assert_eq!(doc["profiles"].as_array().unwrap().len(), 2);
        let entries: Vec<_> = fs::read_dir(repo.path().parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries, vec![FileProfileRepository::FILE_NAME.to_owned()]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(repo.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dir_mode = fs::metadata(repo.path().parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700);
        }
    }

    #[tokio::test]
    async fn unknown_fields_and_future_profiles_survive_round_trip() {
        let (_dir, repo) = repo();
        let known = Uuid::now_v7();
        let future = Uuid::now_v7();
        let doc = serde_json::json!({
            "schema_version": 3,
            "groups": [{"name": "prod", "color": "red"}],
            "profiles": [
                {
                    "schema_version": 9,
                    "id": known.to_string(),
                    "name": "from a newer build",
                    "engine": "postgres",
                    "address": {"kind": "network", "host": "h"},
                    "color": "teal",
                    "tls": {"mode": "require", "pinned_spki": ["abc"]},
                    "ssh": {"host": "b", "user": "u", "auth": {"method": "agent"}, "jump": "j2"}
                },
                {
                    "schema_version": 12,
                    "id": future.to_string(),
                    "name": "unparsable here",
                    "engine": "oracle",
                    "address": {"kind": "wallet", "alias": "x"}
                }
            ]
        });
        fs::create_dir_all(repo.path().parent().unwrap()).unwrap();
        fs::write(repo.path(), serde_json::to_string(&doc).unwrap()).unwrap();

        // The unparsable profile is skipped, not an error.
        let listed = repo.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].schema_version, 9);
        assert_eq!(repo.get(future).await.unwrap(), None);

        // Saving another profile rewrites the file and keeps everything unknown.
        repo.save(&profile("new")).await.unwrap();
        // Saving the known one back (unchanged) keeps its unknown fields.
        repo.save(&listed[0]).await.unwrap();

        let back: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(repo.path()).unwrap()).unwrap();
        assert_eq!(back["schema_version"], 3);
        assert_eq!(back["groups"], doc["groups"]);
        let profiles = back["profiles"].as_array().unwrap();
        assert_eq!(profiles.len(), 3);
        assert_eq!(profiles[0]["schema_version"], 9);
        assert_eq!(profiles[0]["color"], "teal");
        assert_eq!(profiles[0]["tls"]["pinned_spki"][0], "abc");
        assert_eq!(profiles[0]["ssh"]["jump"], "j2");
        assert_eq!(
            profiles[1], doc["profiles"][1],
            "unparsable profile kept verbatim"
        );

        // Deleting by id also works for a profile this build cannot parse.
        assert!(repo.delete(future).await.unwrap());
        assert_eq!(repo.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn invalid_file_is_never_overwritten() {
        let (_dir, repo) = repo();
        fs::create_dir_all(repo.path().parent().unwrap()).unwrap();
        fs::write(repo.path(), "{ not json").unwrap();
        let err = repo.list().await.unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);
        assert!(repo.save(&profile("x")).await.is_err());
        assert_eq!(fs::read_to_string(repo.path()).unwrap(), "{ not json");

        // An empty file is an empty repository.
        fs::write(repo.path(), "\n").unwrap();
        assert_eq!(repo.list().await.unwrap(), Vec::new());
    }

    #[tokio::test]
    async fn concurrent_saves_lose_nothing() {
        let (_dir, repo) = repo();
        let mut tasks = Vec::new();
        for i in 0..16 {
            let repo = repo.clone();
            tasks.push(tokio::spawn(async move {
                repo.save(&profile(&format!("p{i:02}"))).await
            }));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        assert_eq!(repo.list().await.unwrap().len(), 16);
    }

    /// The guarantee behind PLAN.md §5: config files hold key ids, never secret values.
    #[tokio::test]
    async fn no_secret_value_is_ever_written() {
        let (_dir, repo) = repo();
        let secrets = MemorySecretStore::new();
        let p = profile_with_every_secret();
        let keys = p.secret_keys();
        assert_eq!(keys.len(), MARKERS.len() - 1, "every keychain slot is used");
        // Store a marker value for every slot the profile references.
        for (slot, marker) in [
            (SecretSlot::Password, MARKERS[0]),
            (SecretSlot::SshPassword, MARKERS[1]),
            (SecretSlot::SshKeyPassphrase, MARKERS[2]),
            (SecretSlot::SshPrivateKey, MARKERS[3]),
            (SecretSlot::TlsClientKey, MARKERS[4]),
        ] {
            secrets
                .set(&p.secret_key(slot), &Secret::new(marker))
                .await
                .unwrap();
        }
        repo.save(&p).await.unwrap();

        // A password-auth SSH profile too.
        let mut q = profile("ssh password");
        q.ssh = Some(SshSettings {
            host: "bastion".into(),
            port: 22,
            user: "ops".into(),
            auth: SshAuthSettings::Password {
                password: SecretRef::Prompt,
            },
            known_hosts: KnownHostsPolicy::default(),
            connect_timeout_secs: None,
            keepalive_secs: None,
            extra: Extra::new(),
        });
        repo.save(&q).await.unwrap();

        // Secrets smuggled into options are refused.
        let mut smuggle = profile("smuggle");
        smuggle
            .options
            .insert("sslpassword".into(), MARKERS[0].into());
        let err = repo.save(&smuggle).await.unwrap_err();
        assert!(!err.to_string().contains(MARKERS[0]), "{err}");
        let mut harmless = profile("harmless");
        harmless
            .options
            .insert("application_name".into(), "x".into());
        // Secret *references* in unknown fields (an object) are fine, as are harmless strings.
        harmless.extra.insert(
            "proxy_password".into(),
            serde_json::json!({"source": "keychain", "key": "connection/x/proxy_password"}),
        );
        harmless
            .extra
            .insert("color".into(), serde_json::json!("teal"));
        repo.save(&harmless).await.unwrap();

        // Secrets smuggled into unknown fields, at any level and depth, are refused.
        let pem_key = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----",
            MARKERS[4]
        );
        let mut cases: Vec<(&str, ConnectionProfile)> = Vec::new();
        let mut p = profile("smuggle 1");
        p.extra
            .insert("sshPassword".into(), serde_json::json!(MARKERS[1]));
        cases.push(("sshPassword", p));
        let mut p = profile("smuggle 2");
        p.tls
            .extra
            .insert("client_key_secret".into(), serde_json::json!(MARKERS[4]));
        cases.push(("tls.client_key_secret", p));
        let mut p = profile("smuggle 3");
        p.ssh = Some(q_ssh());
        if let Some(ssh) = p.ssh.as_mut() {
            ssh.extra.insert(
                "jump".into(),
                serde_json::json!({"host": "j", "passphrase": MARKERS[2]}),
            );
        }
        cases.push(("ssh.jump.passphrase", p));
        let mut p = profile("smuggle 4");
        p.extra.insert(
            "hosts".into(),
            serde_json::json!([{"tokens": ["a", MARKERS[0]]}]),
        );
        cases.push(("hosts.0.tokens", p));
        let mut p = profile("smuggle 5");
        p.tls
            .extra
            .insert("client_key_pem".into(), serde_json::json!(pem_key));
        cases.push(("tls.client_key_pem", p));
        let mut p = profile("smuggle 6");
        p.tls.ca_cert = Some(PemRef::Inline {
            pem: pem_key.clone(),
        });
        cases.push(("tls.ca_cert", p));
        let mut p = profile("smuggle 7");
        p.tls.client_cert = Some(PemRef::Inline {
            pem: pem_key.clone(),
        });
        p.tls.client_key = Some(KeyRef::File {
            path: "/k.pem".into(),
        });
        cases.push(("tls.client_cert", p));
        let mut p = profile("smuggle 8");
        p.options.insert("sslkey_inline".into(), pem_key);
        cases.push(("sslkey_inline", p));
        for (field, p) in cases {
            let err = repo.save(&p).await.unwrap_err();
            assert_eq!(
                err.category(),
                oxbase_types::ErrorCategory::InvalidInput,
                "{field}: {err}"
            );
            assert!(err.to_string().contains(field), "{field}: {err}");
            assert!(
                MARKERS.iter().all(|m| !err.to_string().contains(m)),
                "{field}: {err}"
            );
        }

        let text = fs::read_to_string(repo.path()).unwrap();
        for marker in MARKERS {
            assert!(
                !text.contains(marker),
                "{marker} written to the file:\n{text}"
            );
        }
        // The keys (not the values) are in the file.
        for key in keys {
            assert!(text.contains(key.as_str()), "{key} missing:\n{text}");
        }
        assert!(text.contains("\"source\": \"prompt\""), "{text}");
        let dbg = format!("{repo:?} {secrets:?}");
        assert!(MARKERS.iter().all(|m| !dbg.contains(m)), "{dbg}");
    }

    /// Minimal SSH settings (agent auth) for smuggling tests.
    fn q_ssh() -> SshSettings {
        SshSettings {
            host: "bastion".into(),
            port: 22,
            user: "ops".into(),
            auth: SshAuthSettings::Agent,
            known_hosts: KnownHostsPolicy::default(),
            connect_timeout_secs: None,
            keepalive_secs: None,
            extra: Extra::new(),
        }
    }

    #[test]
    fn config_dir_is_platform_specific() {
        let name = app_dir_name();
        if cfg!(target_os = "macos") {
            assert_eq!(name, "dev.oxbase.app");
        } else {
            assert_eq!(name, "oxbase");
        }
        // Without the override the directory ends in the app's name (where a home exists).
        if std::env::var_os(CONFIG_DIR_ENV).is_none()
            && let Ok(dir) = app_config_dir()
        {
            assert!(dir.ends_with(name), "{}", dir.display());
            if let Some(base) = dirs::config_dir() {
                assert_eq!(dir, base.join(name));
            }
        }
    }
}
