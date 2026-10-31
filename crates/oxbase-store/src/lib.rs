//! App storage: SQLite app DB, versioned JSON documents and OS-keychain secrets.
//!
//! M1 contracts (see `docs/m1-contracts.md`):
//!
//! - [`SecretStore`]: get/set/delete secrets by [`SecretKey`]. [`MemorySecretStore`] is complete
//!   (tests, CI, headless runs without a keychain); [`KeyringSecretStore`] is the OS keychain
//!   backend (service `dev.oxbase.app`, account = `connection/<profile id>/<slot>`).
//! - [`ConnectionProfile`]: a saved connection. It holds secrets only as references
//!   ([`SecretRef`], [`KeyRef`]), never inline, and persists as versioned JSON that preserves
//!   fields it does not know (`extra`), so documents written by newer builds survive a round trip
//!   through older ones (PLAN.md §5).
//! - [`ProfileRepository`]: list/get/save/delete profiles. [`MemoryProfileRepository`] for
//!   tests; [`FileProfileRepository`] persists them as one JSON file in [`app_config_dir`]
//!   (atomic writes, unknown fields preserved, never a secret value).
//!
//! Turning a profile into driver parameters (resolving secrets, opening tunnels) is the
//! connection manager's job in `oxbase-services`.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use oxbase_types::{Engine, KnownHostsPolicy, Secret, TlsMode};
use oxbase_types::{Error, Result};

mod file_repo;

pub use file_repo::{
    CONFIG_DIR_ENV, FileProfileRepository, PROFILES_FILE_SCHEMA_VERSION, app_config_dir,
};

/// Unknown JSON fields preserved across a load/save round trip.
pub type Extra = serde_json::Map<String, serde_json::Value>;

// ---------------------------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------------------------

/// Identifies one secret in a [`SecretStore`].
///
/// 1 to 200 characters from `A-Z a-z 0-9 . _ - : /`. Keys are not secret: they appear in
/// profiles, logs and errors. Profile secrets use [`SecretKey::for_profile`]:
/// `connection/<profile uuid>/<slot>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SecretKey(String);

impl SecretKey {
    pub const MAX_LEN: usize = 200;

    pub fn new(key: impl Into<String>) -> Result<Self> {
        let key = key.into();
        let valid_char =
            |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/');
        if key.is_empty() || key.len() > Self::MAX_LEN || !key.chars().all(valid_char) {
            return Err(Error::invalid_input(format!(
                "invalid secret key '{key}': use 1-{} characters from A-Z a-z 0-9 . _ - : /",
                Self::MAX_LEN
            )));
        }
        Ok(Self(key))
    }

    /// The key for `slot` of the profile `profile_id`.
    pub fn for_profile(profile_id: Uuid, slot: SecretSlot) -> Self {
        Self(format!("connection/{profile_id}/{}", slot.as_str()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for SecretKey {
    type Error = Error;

    fn try_from(s: String) -> Result<Self> {
        Self::new(s)
    }
}

impl From<SecretKey> for String {
    fn from(k: SecretKey) -> Self {
        k.0
    }
}

/// Which secret of a connection profile a key holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretSlot {
    /// Database password.
    Password,
    /// SSH password.
    SshPassword,
    /// Inline SSH private key (when not read from a file).
    SshPrivateKey,
    /// Passphrase of the SSH private key.
    SshKeyPassphrase,
    /// Inline TLS client private key (when not read from a file).
    TlsClientKey,
}

impl SecretSlot {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::SshPassword => "ssh_password",
            Self::SshPrivateKey => "ssh_private_key",
            Self::SshKeyPassphrase => "ssh_key_passphrase",
            Self::TlsClientKey => "tls_client_key",
        }
    }
}

/// Stores secrets (passwords, passphrases, inline keys) by [`SecretKey`].
///
/// Invariants for every implementation:
/// - values are only ever returned as [`Secret`]; they never appear in errors, logs or `Debug`;
/// - `get` of a missing key is `Ok(None)`, not an error;
/// - `set` overwrites; `delete` returns whether the key existed;
/// - implementations backed by blocking OS APIs run them off the async runtime
///   (`spawn_blocking`).
#[async_trait]
pub trait SecretStore: Send + Sync + fmt::Debug {
    async fn get(&self, key: &SecretKey) -> Result<Option<Secret>>;

    async fn set(&self, key: &SecretKey, value: &Secret) -> Result<()>;

    async fn delete(&self, key: &SecretKey) -> Result<bool>;
}

/// In-memory [`SecretStore`] for tests, CI and runs without an OS keychain.
#[derive(Default)]
pub struct MemorySecretStore {
    entries: Mutex<HashMap<SecretKey, Secret>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored secrets.
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |e| e.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<SecretKey, Secret>>> {
        self.entries
            .lock()
            .map_err(|_| Error::internal("secret store lock poisoned"))
    }
}

impl fmt::Debug for MemorySecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemorySecretStore")
            .field("len", &self.len())
            .finish()
    }
}

#[async_trait]
impl SecretStore for MemorySecretStore {
    async fn get(&self, key: &SecretKey) -> Result<Option<Secret>> {
        Ok(self.lock()?.get(key).cloned())
    }

    async fn set(&self, key: &SecretKey, value: &Secret) -> Result<()> {
        self.lock()?.insert(key.clone(), value.clone());
        Ok(())
    }

    async fn delete(&self, key: &SecretKey) -> Result<bool> {
        Ok(self.lock()?.remove(key).is_some())
    }
}

/// [`SecretStore`] backed by the OS keychain through the `keyring` crate (macOS Keychain,
/// Windows Credential Manager, Secret Service on Linux/BSD).
///
/// Each secret is one keychain entry with service [`KeyringSecretStore::service`] and account
/// = the [`SecretKey`] text (`connection/<profile id>/<slot>` for profile secrets).
///
/// The keychain APIs block (and may show an OS unlock prompt), so every call runs on
/// `tokio::task::spawn_blocking`; a Tokio runtime is required. "No entry" is `Ok(None)` /
/// `Ok(false)`; a keychain that is missing, locked or refuses access is `Error::Unsupported`;
/// other failures are `Error::Internal`. No error message ever contains the secret value.
///
/// # Long values
///
/// The Windows Credential Manager stores at most 2560 bytes per entry (1280 UTF-16 code
/// units), less than an RSA private key in PEM form. A value longer than
/// [`KeyringSecretStore::max_entry_len`] UTF-16 code units (on Windows
/// [`KeyringSecretStore::WINDOWS_MAX_ENTRY_LEN`]; no limit elsewhere) is split into parts stored
/// as accounts `<account>#0`, `<account>#1`, ...; the main entry then holds only a marker with
/// the number of parts. `get` joins them, `set` and `delete` remove parts that are no longer
/// needed. Callers see one secret either way.
#[derive(Debug, Clone)]
pub struct KeyringSecretStore {
    service: String,
    max_entry_len: Option<usize>,
}

impl KeyringSecretStore {
    /// Keychain service name used by the app and the CLI (the app's bundle identifier).
    pub const DEFAULT_SERVICE: &'static str = "dev.oxbase.app";

    /// Longest value (UTF-16 code units) stored in one Windows Credential Manager entry; the
    /// API limit is 1280, minus a margin.
    pub const WINDOWS_MAX_ENTRY_LEN: usize = 1200;

    pub fn new() -> Self {
        Self::with_service(Self::DEFAULT_SERVICE)
    }

    /// A store under another service name (tests use a unique one).
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            max_entry_len: cfg!(windows).then_some(Self::WINDOWS_MAX_ENTRY_LEN),
        }
    }

    /// Splits values longer than `len` UTF-16 code units across several entries (`None`: never
    /// split). The platform default suits the platform; tests use a small value.
    #[must_use]
    pub fn with_max_entry_len(mut self, len: Option<usize>) -> Self {
        self.max_entry_len = len;
        self
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    pub fn max_entry_len(&self) -> Option<usize> {
        self.max_entry_len
    }

    /// Runs `op` on the keychain entries for `key` on the blocking thread pool.
    async fn with_entries<T, F>(&self, key: &SecretKey, op: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&keychain::Entries) -> Result<T> + Send + 'static,
    {
        let entries = keychain::Entries::new(self.service.clone(), key.as_str().to_owned());
        tokio::task::spawn_blocking(move || op(&entries))
            .await
            .map_err(|e| {
                Error::internal(format!(
                    "OS keychain task failed: {}",
                    if e.is_panic() {
                        "panicked"
                    } else {
                        "cancelled"
                    }
                ))
            })?
    }
}

impl Default for KeyringSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SecretStore for KeyringSecretStore {
    async fn get(&self, key: &SecretKey) -> Result<Option<Secret>> {
        self.with_entries(key, keychain::Entries::get).await
    }

    async fn set(&self, key: &SecretKey, value: &Secret) -> Result<()> {
        let value = value.clone();
        let max = self.max_entry_len;
        self.with_entries(key, move |entries| entries.set(&value, max))
            .await
    }

    async fn delete(&self, key: &SecretKey) -> Result<bool> {
        self.with_entries(key, keychain::Entries::delete).await
    }
}

mod keychain {
    use oxbase_types::{Error, Result, Secret};

    /// Main-entry value of a secret split into parts: `oxbase-chunked:v1:<parts>`.
    pub(crate) const CHUNK_PREFIX: &str = "oxbase-chunked:v1:";

    /// At most this many parts (about 300 KB at the Windows entry size).
    pub(crate) const MAX_CHUNKS: usize = 256;

    /// The number of parts if `value` is a split-value marker.
    pub(crate) fn chunk_header(value: &str) -> Option<usize> {
        value
            .strip_prefix(CHUNK_PREFIX)?
            .parse()
            .ok()
            .filter(|n| (1..=MAX_CHUNKS).contains(n))
    }

    /// Splits `value` at character boundaries into parts of at most `max_units` UTF-16 code
    /// units each (at least 2, so every character fits). An empty value has no parts.
    pub(crate) fn split_utf16(value: &str, max_units: usize) -> Vec<&str> {
        let max_units = max_units.max(2);
        let mut parts = Vec::new();
        let mut start = 0;
        let mut units = 0_usize;
        for (i, c) in value.char_indices() {
            let n = c.len_utf16();
            if units.saturating_add(n) > max_units {
                parts.push(&value[start..i]);
                start = i;
                units = 0;
            }
            units += n;
        }
        if start < value.len() {
            parts.push(&value[start..]);
        }
        parts
    }

    /// The keychain entries of one secret: the main account and, for a split value, its parts.
    /// Runs on a blocking thread.
    pub(crate) struct Entries {
        service: String,
        account: String,
    }

    impl Entries {
        pub(crate) fn new(service: String, account: String) -> Self {
            Self { service, account }
        }

        fn part(&self, i: usize) -> String {
            format!("{}#{i}", self.account)
        }

        fn entry(&self, account: &str) -> Result<keyring::Entry> {
            keyring::Entry::new(&self.service, account).map_err(|e| map_error(&e, "open", account))
        }

        /// The value of `account`; a missing entry is `None`.
        fn read(&self, account: &str) -> Result<Option<String>> {
            match self.entry(account)?.get_password() {
                Ok(value) => Ok(Some(value)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(map_error(&e, "read", account)),
            }
        }

        fn write(&self, account: &str, value: &str) -> Result<()> {
            self.entry(account)?
                .set_password(value)
                .map_err(|e| map_error(&e, "write", account))
        }

        /// Deletes `account`; returns whether it existed.
        fn remove(&self, account: &str) -> Result<bool> {
            match self.entry(account)?.delete_credential() {
                Ok(()) => Ok(true),
                Err(keyring::Error::NoEntry) => Ok(false),
                Err(e) => Err(map_error(&e, "delete", account)),
            }
        }

        /// Parts the main entry currently points to (0 for a plain, missing or unreadable one).
        fn current_parts(&self) -> usize {
            match self.read(&self.account) {
                Ok(Some(main)) => chunk_header(&main).unwrap_or(0),
                _ => 0,
            }
        }

        pub(crate) fn get(&self) -> Result<Option<Secret>> {
            let Some(main) = self.read(&self.account)? else {
                return Ok(None);
            };
            let Some(parts) = chunk_header(&main) else {
                return Ok(Some(Secret::new(main)));
            };
            let mut value = String::new();
            for i in 0..parts {
                let Some(part) = self.read(&self.part(i))? else {
                    return Err(Error::internal(format!(
                        "OS keychain: '{}' is incomplete (part {} of {parts} is missing); \
                         enter the secret again",
                        self.account,
                        i + 1
                    )));
                };
                value.push_str(&part);
            }
            Ok(Some(Secret::new(value)))
        }

        pub(crate) fn set(&self, value: &Secret, max_len: Option<usize>) -> Result<()> {
            let value = value.expose();
            let old_parts = self.current_parts();
            // A value that looks like a marker is always split, so it is never misread as one.
            let split = value.starts_with(CHUNK_PREFIX)
                || max_len.is_some_and(|max| value.encode_utf16().count() > max);
            let parts = if split {
                split_utf16(value, max_len.unwrap_or(usize::MAX))
            } else {
                Vec::new()
            };
            if parts.len() > MAX_CHUNKS {
                return Err(Error::invalid_input(format!(
                    "OS keychain: cannot write '{}': the value is too long for the keychain",
                    self.account
                )));
            }
            if parts.is_empty() {
                self.write(&self.account, value)?;
            } else {
                for (i, part) in parts.iter().enumerate() {
                    self.write(&self.part(i), part)?;
                }
                self.write(&self.account, &format!("{CHUNK_PREFIX}{}", parts.len()))?;
            }
            for i in parts.len()..old_parts {
                self.remove(&self.part(i))?;
            }
            Ok(())
        }

        pub(crate) fn delete(&self) -> Result<bool> {
            let old_parts = self.current_parts();
            let existed = self.remove(&self.account)?;
            for i in 0..old_parts {
                self.remove(&self.part(i))?;
            }
            Ok(existed)
        }
    }

    /// Maps a `keyring` error to an oxbase error. Never formats the error's payload where it may
    /// hold secret bytes (`BadEncoding`, `BadDataFormat`) or other entries (`Ambiguous`).
    pub(crate) fn map_error(e: &keyring::Error, action: &str, account: &str) -> Error {
        use keyring::Error as K;
        let what = format!("OS keychain: cannot {action} '{account}'");
        match e {
            K::NoDefaultStore => Error::unsupported(format!(
                "{what}: no OS keychain is available (on Linux a Secret Service such as \
                 GNOME Keyring or KWallet must be running)"
            )),
            K::NoStorageAccess(platform) => Error::unsupported(format!(
                "{what}: the keychain is locked or access was denied ({platform})"
            )),
            K::NotSupportedByStore(why) => Error::unsupported(format!("{what}: {why}")),
            K::NoEntry => Error::not_found(format!("{what}: no such entry")),
            K::BadEncoding(_) => Error::internal(format!("{what}: stored value is not UTF-8")),
            K::BadDataFormat(..) => {
                Error::internal(format!("{what}: stored value has an unexpected format"))
            }
            K::BadStoreFormat(why) => Error::internal(format!("{what}: {why}")),
            K::TooLong(attr, limit) => Error::invalid_input(format!(
                "{what}: {attr} is longer than the keychain limit of {limit}"
            )),
            K::Invalid(param, why) => Error::invalid_input(format!("{what}: {param} {why}")),
            K::Ambiguous(entries) => Error::internal(format!(
                "{what}: {} matching entries exist; remove the duplicates",
                entries.len()
            )),
            K::PlatformFailure(platform) => Error::internal(format!("{what}: {platform}")),
            // `keyring::Error` is non-exhaustive.
            _ => Error::internal(format!("{what}: unexpected keychain error")),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Connection profiles
// ---------------------------------------------------------------------------------------------

/// Current [`ConnectionProfile::schema_version`]. Bump it when the meaning of an existing field
/// changes; adding optional fields does not need a bump.
pub const PROFILE_SCHEMA_VERSION: u32 = 1;

fn current_schema_version() -> u32 {
    PROFILE_SCHEMA_VERSION
}

fn default_ssh_port() -> u16 {
    22
}

/// Where a secret comes from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum SecretRef {
    /// Stored in the [`SecretStore`] under `key`.
    Keychain { key: SecretKey },
    /// Not saved: the user is asked on every connect (the manager gets it from the caller).
    Prompt,
}

/// Public PEM material (CA bundle, client certificate).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum PemRef {
    File {
        path: PathBuf,
    },
    /// Certificates are public, so they may be stored inline.
    Inline {
        pem: String,
    },
}

/// Private key material: a file on disk or a secret in the [`SecretStore`]. Never inline.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum KeyRef {
    File { path: PathBuf },
    Keychain { key: SecretKey },
}

/// Where the server is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProfileAddress {
    /// A network server. This is the *server* endpoint: with SSH it is resolved by the SSH
    /// server, and TLS verifies it (unless `tls.server_name` overrides it).
    Network {
        host: String,
        /// `None`: the engine's default port.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
    },
    /// A database file (SQLite).
    File { path: PathBuf },
}

/// Serializable TLS settings (runtime form: `oxbase_types::TlsOptions`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TlsSettings {
    #[serde(default)]
    pub mode: TlsMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<PemRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert: Option<PemRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<KeyRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// How to authenticate to the SSH server (runtime form: `oxbase_types::SshAuth`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum SshAuthSettings {
    Password {
        password: SecretRef,
    },
    PrivateKey {
        key: KeyRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        passphrase: Option<SecretRef>,
    },
    Agent,
}

/// Serializable SSH tunnel settings (runtime form: `oxbase_types::SshOptions`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SshSettings {
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub user: String,
    pub auth: SshAuthSettings,
    #[serde(default)]
    pub known_hosts: KnownHostsPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_timeout_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive_secs: Option<u32>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// A saved connection.
///
/// Secrets are referenced ([`SecretRef`] / [`KeyRef`]), never stored inline. The JSON form is
/// versioned ([`ConnectionProfile::schema_version`]) and forward compatible: fields this build
/// does not know are kept in `extra` (here and in the nested TLS/SSH settings) and written back
/// unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    /// Document version; see [`PROFILE_SCHEMA_VERSION`]. Newer versions load (their unknown
    /// fields are preserved) and are saved with their version unchanged.
    #[serde(default = "current_schema_version")]
    pub schema_version: u32,
    pub id: Uuid,
    pub name: String,
    pub engine: Engine,
    pub address: ProfileAddress,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<SecretRef>,
    #[serde(default)]
    pub tls: TlsSettings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_timeout_secs: Option<u32>,
    /// Engine-specific options passed to the driver. Never put secrets here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl ConnectionProfile {
    /// A new profile with a fresh (time-ordered) id and default settings.
    pub fn new(name: impl Into<String>, engine: Engine, address: ProfileAddress) -> Self {
        Self {
            schema_version: PROFILE_SCHEMA_VERSION,
            id: Uuid::now_v7(),
            name: name.into(),
            engine,
            address,
            database: None,
            user: None,
            password: None,
            tls: TlsSettings::default(),
            ssh: None,
            connect_timeout_secs: None,
            options: BTreeMap::new(),
            extra: Extra::new(),
        }
    }

    /// The [`SecretKey`] for `slot` of this profile.
    pub fn secret_key(&self, slot: SecretSlot) -> SecretKey {
        SecretKey::for_profile(self.id, slot)
    }

    /// Every keychain key this profile refers to (to delete them with the profile).
    pub fn secret_keys(&self) -> Vec<SecretKey> {
        fn from_secret(r: Option<&SecretRef>, out: &mut Vec<SecretKey>) {
            if let Some(SecretRef::Keychain { key }) = r {
                out.push(key.clone());
            }
        }
        fn from_key(r: Option<&KeyRef>, out: &mut Vec<SecretKey>) {
            if let Some(KeyRef::Keychain { key }) = r {
                out.push(key.clone());
            }
        }
        let mut out = Vec::new();
        from_secret(self.password.as_ref(), &mut out);
        from_key(self.tls.client_key.as_ref(), &mut out);
        if let Some(ssh) = &self.ssh {
            match &ssh.auth {
                SshAuthSettings::Password { password } => from_secret(Some(password), &mut out),
                SshAuthSettings::PrivateKey { key, passphrase } => {
                    from_key(Some(key), &mut out);
                    from_secret(passphrase.as_ref(), &mut out);
                }
                SshAuthSettings::Agent => {}
            }
        }
        out
    }

    /// Checks the profile is consistent: a host for network engines, a file for SQLite, no SSH
    /// for SQLite, client certificate and key together, a non-empty name.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(Error::invalid_input("connection name must not be empty"));
        }
        match (&self.address, self.engine.is_networked()) {
            (ProfileAddress::Network { host, .. }, true) if !host.trim().is_empty() => {}
            (ProfileAddress::File { path }, false) if !path.as_os_str().is_empty() => {}
            _ => {
                return Err(Error::invalid_input(format!(
                    "{} needs {}",
                    self.engine.display_name(),
                    if self.engine.is_networked() {
                        "a host"
                    } else {
                        "a database file"
                    }
                )));
            }
        }
        if self.ssh.is_some() && !self.engine.is_networked() {
            return Err(Error::invalid_input(format!(
                "{} cannot use an SSH tunnel",
                self.engine.display_name()
            )));
        }
        if let Some(ssh) = &self.ssh
            && (ssh.host.trim().is_empty() || ssh.user.trim().is_empty() || ssh.port == 0)
        {
            return Err(Error::invalid_input(
                "SSH tunnel needs a host, a port and a user",
            ));
        }
        if self.tls.client_cert.is_some() != self.tls.client_key.is_some() {
            return Err(Error::invalid_input(
                "TLS client certificate and key must be given together",
            ));
        }
        Ok(())
    }

    /// Serializes to pretty JSON.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| Error::internal(format!("cannot serialize connection profile: {e}")))
    }

    /// Parses JSON written by this or any other (older or newer) build.
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json)
            .map_err(|e| Error::invalid_input(format!("invalid connection profile: {e}")))
    }
}

/// Persists [`ConnectionProfile`]s.
///
/// `save` inserts or replaces by `id` and must keep `extra` intact; `list` is ordered by name
/// (case-insensitive), then id. Deleting a profile does not delete its secrets: the caller
/// deletes [`ConnectionProfile::secret_keys`] from the [`SecretStore`].
#[async_trait]
pub trait ProfileRepository: Send + Sync + fmt::Debug {
    async fn list(&self) -> Result<Vec<ConnectionProfile>>;

    async fn get(&self, id: Uuid) -> Result<Option<ConnectionProfile>>;

    async fn save(&self, profile: &ConnectionProfile) -> Result<()>;

    /// Returns whether the profile existed.
    async fn delete(&self, id: Uuid) -> Result<bool>;
}

/// The [`ProfileRepository::list`] order: name (case-insensitive), then id.
fn sort_profiles(all: &mut [ConnectionProfile]) {
    all.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.id.cmp(&b.id))
    });
}

/// In-memory [`ProfileRepository`] for tests.
#[derive(Debug, Default)]
pub struct MemoryProfileRepository {
    profiles: Mutex<HashMap<Uuid, ConnectionProfile>>,
}

impl MemoryProfileRepository {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<Uuid, ConnectionProfile>>> {
        self.profiles
            .lock()
            .map_err(|_| Error::internal("profile repository lock poisoned"))
    }
}

#[async_trait]
impl ProfileRepository for MemoryProfileRepository {
    async fn list(&self) -> Result<Vec<ConnectionProfile>> {
        let mut all: Vec<_> = self.lock()?.values().cloned().collect();
        sort_profiles(&mut all);
        Ok(all)
    }

    async fn get(&self, id: Uuid) -> Result<Option<ConnectionProfile>> {
        Ok(self.lock()?.get(&id).cloned())
    }

    async fn save(&self, profile: &ConnectionProfile) -> Result<()> {
        profile.validate()?;
        self.lock()?.insert(profile.id, profile.clone());
        Ok(())
    }

    async fn delete(&self, id: Uuid) -> Result<bool> {
        Ok(self.lock()?.remove(&id).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn sample() -> ConnectionProfile {
        let mut p = ConnectionProfile::new(
            "prod pg",
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
        p.tls = TlsSettings {
            mode: TlsMode::VerifyFull,
            ca_cert: Some(PemRef::File {
                path: "/etc/ssl/db-ca.pem".into(),
            }),
            ..TlsSettings::default()
        };
        p.ssh = Some(SshSettings {
            host: "bastion".into(),
            port: 22,
            user: "ops".into(),
            auth: SshAuthSettings::PrivateKey {
                key: KeyRef::File {
                    path: "/home/ops/.ssh/id_ed25519".into(),
                },
                passphrase: Some(SecretRef::Keychain {
                    key: p.secret_key(SecretSlot::SshKeyPassphrase),
                }),
            },
            known_hosts: KnownHostsPolicy::default(),
            connect_timeout_secs: None,
            keepalive_secs: Some(30),
            extra: Extra::new(),
        });
        p
    }

    #[test]
    fn secret_keys_validate() {
        assert!(SecretKey::new("connection/abc/password").is_ok());
        assert!(SecretKey::new("").is_err());
        assert!(SecretKey::new("has space").is_err());
        assert!(SecretKey::new("x".repeat(201)).is_err());
        let id = Uuid::nil();
        assert_eq!(
            SecretKey::for_profile(id, SecretSlot::SshKeyPassphrase).as_str(),
            "connection/00000000-0000-0000-0000-000000000000/ssh_key_passphrase"
        );
        assert!(serde_json::from_str::<SecretKey>("\"bad key\"").is_err());
    }

    #[test]
    fn profile_round_trip_and_secret_refs() {
        let p = sample();
        p.validate().unwrap();
        let json = p.to_json().unwrap();
        assert!(json.contains("\"schema_version\": 1"), "{json}");
        assert!(json.contains("\"mode\": \"verify_full\""), "{json}");
        let back = ConnectionProfile::from_json(&json).unwrap();
        assert_eq!(back, p);
        assert_eq!(
            p.secret_keys(),
            vec![
                p.secret_key(SecretSlot::Password),
                p.secret_key(SecretSlot::SshKeyPassphrase)
            ]
        );
    }

    #[test]
    fn unknown_fields_survive_round_trip() {
        let id = Uuid::now_v7();
        let json = format!(
            r#"{{
              "schema_version": 7,
              "id": "{id}",
              "name": "from the future",
              "engine": "mysql",
              "address": {{"kind": "network", "host": "h"}},
              "color": "teal",
              "tls": {{"mode": "require", "pinned_spki": ["abc"]}},
              "ssh": {{"host": "b", "user": "u", "auth": {{"method": "agent"}}, "jump": "j2"}}
            }}"#
        );
        let p = ConnectionProfile::from_json(&json).unwrap();
        assert_eq!(p.schema_version, 7);
        assert_eq!(p.extra.get("color"), Some(&serde_json::json!("teal")));
        assert_eq!(p.tls.mode, TlsMode::Require);
        assert!(p.tls.extra.contains_key("pinned_spki"));
        let ssh = p.ssh.as_ref().unwrap();
        assert_eq!(ssh.port, 22);
        assert_eq!(ssh.extra.get("jump"), Some(&serde_json::json!("j2")));

        let again: serde_json::Value = serde_json::from_str(&p.to_json().unwrap()).unwrap();
        assert_eq!(again["color"], "teal");
        assert_eq!(again["schema_version"], 7);
        assert_eq!(again["tls"]["pinned_spki"][0], "abc");
        assert_eq!(again["ssh"]["jump"], "j2");
    }

    #[test]
    fn minimal_profile_gets_defaults() {
        let p = ConnectionProfile::from_json(
            r#"{"id":"00000000-0000-0000-0000-000000000001","name":"local",
                "engine":"sqlite","address":{"kind":"file","path":"/tmp/x.db"}}"#,
        )
        .unwrap();
        assert_eq!(p.schema_version, PROFILE_SCHEMA_VERSION);
        assert_eq!(p.tls.mode, TlsMode::Prefer);
        p.validate().unwrap();
        let mut bad = p.clone();
        bad.ssh = sample().ssh;
        assert!(bad.validate().is_err());
    }

    #[tokio::test]
    async fn memory_secret_store() {
        let store = MemorySecretStore::new();
        let key = SecretKey::new("connection/x/password").unwrap();
        assert_eq!(store.get(&key).await.unwrap(), None);
        store.set(&key, &Secret::new("s3cr3t-value")).await.unwrap();
        assert_eq!(
            store.get(&key).await.unwrap().unwrap().expose(),
            "s3cr3t-value"
        );
        assert!(!format!("{store:?}").contains("s3cr3t"));
        assert!(store.delete(&key).await.unwrap());
        assert!(!store.delete(&key).await.unwrap());
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn memory_secret_store_overwrites_isolates_and_redacts() {
        let store = MemorySecretStore::new();
        let a = SecretKey::for_profile(Uuid::now_v7(), SecretSlot::Password);
        let b = SecretKey::for_profile(Uuid::now_v7(), SecretSlot::Password);
        store.set(&a, &Secret::new("first-MARKER")).await.unwrap();
        store.set(&a, &Secret::new("second-MARKER")).await.unwrap();
        store.set(&b, &Secret::new("other-MARKER")).await.unwrap();
        assert_eq!(store.len(), 2);
        assert_eq!(
            store.get(&a).await.unwrap(),
            Some(Secret::new("second-MARKER"))
        );
        assert!(store.delete(&a).await.unwrap());
        assert_eq!(store.get(&a).await.unwrap(), None);
        assert_eq!(
            store.get(&b).await.unwrap(),
            Some(Secret::new("other-MARKER"))
        );
        let dbg = format!("{store:?} {:?}", store.get(&b).await.unwrap());
        assert!(!dbg.contains("MARKER"), "{dbg}");

        // Usable as a shared trait object from many tasks.
        let shared: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(store);
        let mut tasks = Vec::new();
        for i in 0..8 {
            let shared = shared.clone();
            tasks.push(tokio::spawn(async move {
                let key = SecretKey::new(format!("test/{i}")).unwrap();
                shared
                    .set(&key, &Secret::new(format!("v{i}")))
                    .await
                    .unwrap();
                shared.get(&key).await.unwrap().unwrap().expose().to_owned()
            }));
        }
        for (i, t) in tasks.into_iter().enumerate() {
            assert_eq!(t.await.unwrap(), format!("v{i}"));
        }
    }

    #[test]
    fn keyring_service_and_error_mapping() {
        use oxbase_types::ErrorCategory;
        assert_eq!(KeyringSecretStore::new().service(), "dev.oxbase.app");
        assert_eq!(KeyringSecretStore::with_service("x").service(), "x");
        let secret_bytes = b"MARKER-secret-bytes".to_vec();
        let cases = [
            (keyring::Error::NoDefaultStore, ErrorCategory::Unsupported),
            (
                keyring::Error::NotSupportedByStore("read-only".into()),
                ErrorCategory::Unsupported,
            ),
            (
                keyring::Error::BadEncoding(secret_bytes.clone()),
                ErrorCategory::Internal,
            ),
            (
                keyring::Error::Invalid("user".into(), "is empty".into()),
                ErrorCategory::InvalidInput,
            ),
            (
                keyring::Error::TooLong("user".into(), 10),
                ErrorCategory::InvalidInput,
            ),
            (keyring::Error::NoEntry, ErrorCategory::NotFound),
        ];
        for (e, category) in cases {
            let mapped = keychain::map_error(&e, "read", "connection/x/password");
            assert_eq!(mapped.category(), category, "{mapped}");
            let text = format!("{mapped} {mapped:?}");
            assert!(text.contains("connection/x/password"), "{text}");
            assert!(!text.contains("MARKER"), "{text}");
        }
    }

    /// Real OS keychain round trip. Needs a desktop session (an unlocked login keychain, the
    /// Windows Credential Manager, or a running Secret Service on Linux):
    /// `cargo test -p oxbase-store -- --ignored keychain_smoke`.
    #[tokio::test]
    #[ignore = "needs an OS keychain (desktop session)"]
    async fn keychain_smoke() {
        let store = KeyringSecretStore::with_service(format!(
            "{}.test-{}",
            KeyringSecretStore::DEFAULT_SERVICE,
            Uuid::new_v4().simple()
        ));
        let key = SecretKey::for_profile(Uuid::now_v7(), SecretSlot::Password);
        assert_eq!(store.get(&key).await.unwrap(), None);
        assert!(!store.delete(&key).await.unwrap());
        store
            .set(&key, &Secret::new("smoke-1 \u{e9}"))
            .await
            .unwrap();
        store.set(&key, &Secret::new("smoke-2")).await.unwrap();
        assert_eq!(store.get(&key).await.unwrap(), Some(Secret::new("smoke-2")));
        assert!(store.delete(&key).await.unwrap());
        assert_eq!(store.get(&key).await.unwrap(), None);
    }

    #[test]
    fn long_keychain_values_split_at_character_boundaries() {
        let value = format!("{}\u{e9}\u{1f511}{}", "a".repeat(9), "b".repeat(4000));
        for max in [2, 3, 10, 1200] {
            let parts = keychain::split_utf16(&value, max);
            assert!(parts.len() > 1, "{max}");
            assert!(
                parts.iter().all(|p| p.encode_utf16().count() <= max),
                "{max}"
            );
            assert_eq!(parts.concat(), value, "{max}");
        }
        assert_eq!(keychain::split_utf16("short", usize::MAX), vec!["short"]);
        assert_eq!(keychain::split_utf16("", 10), Vec::<&str>::new());

        assert_eq!(keychain::chunk_header("oxbase-chunked:v1:3"), Some(3));
        for not_a_header in [
            "oxbase-chunked:v1:0",
            "oxbase-chunked:v1:x",
            "oxbase-chunked:v1:100000",
            "password",
            "",
        ] {
            assert_eq!(keychain::chunk_header(not_a_header), None, "{not_a_header}");
        }
        assert_eq!(
            KeyringSecretStore::new().max_entry_len(),
            cfg!(windows).then_some(KeyringSecretStore::WINDOWS_MAX_ENTRY_LEN)
        );
    }

    /// Real OS keychain round trip of values longer than one entry (an RSA-4096 PEM is about
    /// 3.3 KB): forced small parts here, the platform default on Windows CI.
    /// `cargo test -p oxbase-store -- --ignored keychain_long_values`.
    #[tokio::test]
    #[ignore = "needs an OS keychain (desktop session)"]
    async fn keychain_long_values() {
        let service = format!(
            "{}.test-{}",
            KeyringSecretStore::DEFAULT_SERVICE,
            Uuid::new_v4().simple()
        );
        let long = format!("-----BEGIN PRIVATE KEY-----\n{}\n", "k".repeat(4096));
        for store in [
            KeyringSecretStore::with_service(&service),
            KeyringSecretStore::with_service(&service).with_max_entry_len(Some(700)),
        ] {
            let key = SecretKey::for_profile(Uuid::now_v7(), SecretSlot::SshPrivateKey);
            store.set(&key, &Secret::new(long.clone())).await.unwrap();
            assert_eq!(
                store.get(&key).await.unwrap(),
                Some(Secret::new(long.clone()))
            );
            // Shrinking to a plain value removes the parts; a marker-like value round-trips.
            store.set(&key, &Secret::new("short")).await.unwrap();
            assert_eq!(store.get(&key).await.unwrap(), Some(Secret::new("short")));
            let tricky = "oxbase-chunked:v1:2";
            store.set(&key, &Secret::new(tricky)).await.unwrap();
            assert_eq!(store.get(&key).await.unwrap(), Some(Secret::new(tricky)));
            store.set(&key, &Secret::new(long.clone())).await.unwrap();
            assert!(store.delete(&key).await.unwrap());
            assert_eq!(store.get(&key).await.unwrap(), None);
            // No part is left behind.
            let part = keychain::Entries::new(service.clone(), format!("{key}#0"));
            assert_eq!(part.get().unwrap(), None);
        }
    }

    #[tokio::test]
    async fn memory_profile_repository() {
        let repo = MemoryProfileRepository::new();
        let a = sample();
        let mut b = sample();
        b.name = "Alpha".into();
        repo.save(&a).await.unwrap();
        repo.save(&b).await.unwrap();
        let names: Vec<_> = repo
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["Alpha", "prod pg"]);
        assert_eq!(repo.get(a.id).await.unwrap(), Some(a.clone()));
        assert!(repo.delete(a.id).await.unwrap());
        assert_eq!(repo.get(a.id).await.unwrap(), None);
        let mut invalid = sample();
        invalid.name = " ".into();
        assert!(repo.save(&invalid).await.is_err());
    }
}
