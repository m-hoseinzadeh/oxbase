//! Saving, testing and deleting connection profiles together with their keychain secrets. Shared
//! by the desktop app and `oxbase-cli`, so both enforce the same rules.
//!
//! A front end sends a [`ConnectionProfile`] (secrets only as references) and, separately, the
//! secret values the user typed ([`ProvidedSecrets`]). Whether a typed value is kept is decided by
//! the profile's reference for that slot: [`SecretRef::Keychain`] stores it in the
//! [`SecretStore`], [`SecretRef::Prompt`] (or no reference) uses it for the current call only.
//! Secret values are never written into the profile.
//!
//! # Stored passwords stay with their destination
//!
//! A stored secret that is *sent* to a server (the database password and the SSH password) only
//! ever goes where the saved profile sent it when it was stored: the same engine, server address,
//! user, TLS settings, SSH server, SSH user and host-key policy ([`same_destination`]).
//!
//! - [`save_profile`] refuses an edit that changes the destination unless those secrets are
//!   typed again (`InvalidInput`), so a typo or a compromised front end cannot re-point a saved
//!   profile at another host and then open it with the old password.
//! - [`restrict_to_saved_destination`] (used before testing an unsaved, edited profile) turns
//!   those keychain references into prompts when the destination differs from the saved one.
//!
//! Key passphrases (used locally to decrypt a key) and key material (only signs) are not sent and
//! are not affected.

use oxbase_store::{
    ConnectionProfile, KeyRef, ProfileRepository, SecretKey, SecretRef, SecretSlot, SecretStore,
    SshAuthSettings,
};
use oxbase_types::{Error, Result, Secret};
use uuid::Uuid;

use crate::ProvidedSecrets;
use crate::schema_cache::SchemaCacheService;

/// The typed, non-empty value for `slot`, if any.
fn provided_value(provided: &ProvidedSecrets, slot: SecretSlot) -> Option<Secret> {
    let value = match slot {
        SecretSlot::Password => provided.password.as_ref(),
        SecretSlot::SshPassword => provided.ssh_password.as_ref(),
        SecretSlot::SshKeyPassphrase => provided.ssh_key_passphrase.as_ref(),
        SecretSlot::SshPrivateKey | SecretSlot::TlsClientKey => None,
    };
    value.filter(|v| !v.is_empty()).cloned()
}

/// Whether the secret in `slot` is sent to a server (and so must stay with its destination).
fn is_sent(slot: SecretSlot) -> bool {
    matches!(slot, SecretSlot::Password | SecretSlot::SshPassword)
}

/// Human name of a slot, for error messages.
fn slot_label(slot: SecretSlot) -> &'static str {
    match slot {
        SecretSlot::Password => "the database password",
        SecretSlot::SshPassword => "the SSH password",
        SecretSlot::SshPrivateKey => "the SSH private key",
        SecretSlot::SshKeyPassphrase => "the SSH key passphrase",
        SecretSlot::TlsClientKey => "the TLS client key",
    }
}

/// The keychain keys `profile` refers to, by slot, after [`normalize_secret_refs`].
fn keychain_slots(profile: &ConnectionProfile) -> Vec<(SecretSlot, SecretKey)> {
    let mut out = Vec::new();
    if let Some(SecretRef::Keychain { key }) = &profile.password {
        out.push((SecretSlot::Password, key.clone()));
    }
    if let Some(KeyRef::Keychain { key }) = &profile.tls.client_key {
        out.push((SecretSlot::TlsClientKey, key.clone()));
    }
    if let Some(ssh) = &profile.ssh {
        match &ssh.auth {
            SshAuthSettings::Password {
                password: SecretRef::Keychain { key },
            } => out.push((SecretSlot::SshPassword, key.clone())),
            SshAuthSettings::PrivateKey { key, passphrase } => {
                if let KeyRef::Keychain { key } = key {
                    out.push((SecretSlot::SshPrivateKey, key.clone()));
                }
                if let Some(SecretRef::Keychain { key }) = passphrase {
                    out.push((SecretSlot::SshKeyPassphrase, key.clone()));
                }
            }
            SshAuthSettings::Password { .. } | SshAuthSettings::Agent => {}
        }
    }
    out
}

/// Whether `key` is in the profile `id`'s own keychain namespace (`connection/<id>/...`). Only
/// such keys are ever deleted on the profile's behalf: a hand-edited or imported profile may
/// point at a key that other profiles share.
fn is_own_key(id: Uuid, key: &SecretKey) -> bool {
    key.as_str()
        .strip_prefix("connection/")
        .and_then(|rest| rest.strip_prefix(id.to_string().as_str()))
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether `candidate` sends its credentials to the same place, the same way, as the saved
/// profile `saved`: same engine, server address, user and TLS settings, and the same SSH server,
/// user and host-key policy.
pub fn same_destination(saved: &ConnectionProfile, candidate: &ConnectionProfile) -> bool {
    let ssh_route = |p: &ConnectionProfile| {
        p.ssh.as_ref().map(|ssh| {
            (
                ssh.host.clone(),
                ssh.port,
                ssh.user.clone(),
                ssh.known_hosts.clone(),
            )
        })
    };
    let tls = |p: &ConnectionProfile| {
        (
            p.tls.mode,
            p.tls.ca_cert.clone(),
            p.tls.client_cert.clone(),
            p.tls.client_key.clone(),
            p.tls.server_name.clone(),
        )
    };
    saved.engine == candidate.engine
        && saved.address == candidate.address
        && saved.user == candidate.user
        && tls(saved) == tls(candidate)
        && ssh_route(saved) == ssh_route(candidate)
}

/// Turns the keychain references for secrets that are *sent* to a server (the database password
/// and the SSH password) into [`SecretRef::Prompt`], so only values typed for this call are used.
/// Key passphrases (used locally) and key material (only signs) are left alone.
pub fn prompt_for_sent_secrets(profile: &mut ConnectionProfile) {
    if matches!(profile.password, Some(SecretRef::Keychain { .. })) {
        profile.password = Some(SecretRef::Prompt);
    }
    if let Some(ssh) = &mut profile.ssh
        && matches!(
            ssh.auth,
            SshAuthSettings::Password {
                password: SecretRef::Keychain { .. }
            }
        )
    {
        ssh.auth = SshAuthSettings::Password {
            password: SecretRef::Prompt,
        };
    }
}

/// Points every keychain reference of `profile` at the profile's own key for that slot
/// (`connection/<id>/<slot>`), so a profile can never read or overwrite another profile's
/// secrets, and front ends do not need to compute keys.
pub fn normalize_secret_refs(profile: &mut ConnectionProfile) {
    let id = profile.id;
    let own = |slot| SecretKey::for_profile(id, slot);
    if let Some(SecretRef::Keychain { key }) = &mut profile.password {
        *key = own(SecretSlot::Password);
    }
    if let Some(KeyRef::Keychain { key }) = &mut profile.tls.client_key {
        *key = own(SecretSlot::TlsClientKey);
    }
    if let Some(ssh) = &mut profile.ssh {
        match &mut ssh.auth {
            SshAuthSettings::Password { password } => {
                if let SecretRef::Keychain { key } = password {
                    *key = own(SecretSlot::SshPassword);
                }
            }
            SshAuthSettings::PrivateKey { key, passphrase } => {
                if let KeyRef::Keychain { key } = key {
                    *key = own(SecretSlot::SshPrivateKey);
                }
                if let Some(SecretRef::Keychain { key }) = passphrase {
                    *key = own(SecretSlot::SshKeyPassphrase);
                }
            }
            SshAuthSettings::Agent => {}
        }
    }
}

/// Prepares a possibly edited, unsaved `profile` for a test connect: normalizes its keychain
/// references and, unless it goes to the same destination as the saved profile with its id,
/// turns the references for sent secrets into prompts ([`prompt_for_sent_secrets`]). A test of an
/// edited profile with another server, user, TLS or SSH route therefore uses only values typed
/// for that call.
pub async fn restrict_to_saved_destination(
    repo: &dyn ProfileRepository,
    profile: &mut ConnectionProfile,
) -> Result<()> {
    normalize_secret_refs(profile);
    let saved = repo.get(profile.id).await?;
    if !saved
        .as_ref()
        .is_some_and(|saved| same_destination(saved, profile))
    {
        prompt_for_sent_secrets(profile);
    }
    Ok(())
}

/// Saves `profile` (insert or update) and the typed secrets its keychain references ask for.
///
/// - A nil id means "new": the profile gets a fresh id.
/// - A keychain reference with a typed value stores the value; without one, the secret must
///   already be in the store (editing without retyping), otherwise this is `InvalidInput`.
/// - Secrets sent to a server (database and SSH password) must be typed whenever the profile is
///   new or its destination changed ([`same_destination`]); a stored value is never carried over
///   to another host, user, TLS or SSH route (`InvalidInput` asking to retype it).
/// - Typed values for prompt-only slots are not stored.
/// - Keychain entries (in the profile's own namespace) the previous version referred to and
///   this one does not are deleted (best effort).
/// - If writing the profile fails, secrets stored for slots the previous version did not use are
///   deleted again (best effort), so a failed save of a new profile leaves nothing behind.
///
/// Returns the saved profile (which never contains secret values).
pub async fn save_profile(
    repo: &dyn ProfileRepository,
    secrets: &dyn SecretStore,
    mut profile: ConnectionProfile,
    provided: &ProvidedSecrets,
) -> Result<ConnectionProfile> {
    if profile.id.is_nil() {
        profile.id = Uuid::now_v7();
    }
    normalize_secret_refs(&mut profile);
    profile.validate()?;

    let previous = repo.get(profile.id).await?;
    let moved = previous
        .as_ref()
        .is_some_and(|previous| !same_destination(previous, &profile));
    let wanted = keychain_slots(&profile);
    let mut to_store = Vec::new();
    for (slot, key) in &wanted {
        match provided_value(provided, *slot) {
            Some(value) => to_store.push((key, value)),
            None if moved && is_sent(*slot) => {
                return Err(Error::invalid_input(format!(
                    "retype {}: the server, user, TLS or SSH settings changed, so the saved one \
                     is not used",
                    slot_label(*slot)
                )));
            }
            // Editing without retyping keeps the stored value. A new profile never adopts a sent
            // secret left behind under its id (for example by a delete whose cleanup failed).
            None if (previous.is_some() || !is_sent(*slot))
                && secrets.get(key).await?.is_some() => {}
            None => {
                return Err(Error::invalid_input(format!(
                    "{} is set to be saved, but none was entered",
                    slot_label(*slot)
                )));
            }
        }
    }

    let previous_keys = previous
        .as_ref()
        .map(ConnectionProfile::secret_keys)
        .unwrap_or_default();
    // Keys this save adds (not referenced before): removed again if the save fails.
    let added: Vec<&SecretKey> = to_store
        .iter()
        .map(|(key, _)| *key)
        .filter(|key| !previous_keys.contains(key))
        .collect();
    if let Err(error) = write_secrets_then_profile(repo, secrets, &profile, &to_store).await {
        for key in added {
            let _ = secrets.delete(key).await;
        }
        return Err(error);
    }

    for stale in previous_keys {
        let kept = wanted.iter().any(|(_, key)| *key == stale);
        if !kept && is_own_key(profile.id, &stale) {
            // Best effort: a leftover entry is harmless and the profile is already saved.
            let _ = secrets.delete(&stale).await;
        }
    }
    Ok(profile)
}

/// Stores `to_store`, then writes `profile`; stops at the first error.
async fn write_secrets_then_profile(
    repo: &dyn ProfileRepository,
    secrets: &dyn SecretStore,
    profile: &ConnectionProfile,
    to_store: &[(&SecretKey, Secret)],
) -> Result<()> {
    for (key, value) in to_store {
        secrets.set(key, value).await?;
    }
    repo.save(profile).await
}

/// Deletes the profile `id` and its keychain secrets (only keys in its own namespace). Returns
/// whether the profile existed. Once the profile is gone, deleting its secrets is best effort: a
/// keychain error does not turn a completed delete into a failure.
pub async fn delete_profile(
    repo: &dyn ProfileRepository,
    secrets: &dyn SecretStore,
    id: Uuid,
) -> Result<bool> {
    let Some(profile) = repo.get(id).await? else {
        return Ok(false);
    };
    let existed = repo.delete(id).await?;
    for key in profile.secret_keys() {
        if is_own_key(id, &key) {
            let _ = secrets.delete(&key).await;
        }
    }
    Ok(existed)
}

/// M6: [`delete_profile`], then the profile's schema caches and their persisted snapshots
/// (`SchemaCacheService::forget_profile`, best effort: the profile is gone either way).
pub async fn delete_profile_and_caches(
    repo: &dyn ProfileRepository,
    secrets: &dyn SecretStore,
    caches: &SchemaCacheService,
    id: Uuid,
) -> Result<bool> {
    let existed = delete_profile(repo, secrets, id).await?;
    if let Err(e) = caches.forget_profile(id).await {
        tracing::debug!(category = %e.category(), "dropping a profile's schema caches failed");
    }
    Ok(existed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_store::{
        Engine, KnownHostsPolicy, MemoryProfileRepository, MemorySecretStore, ProfileAddress,
        SshSettings,
    };
    use oxbase_types::ErrorCategory;

    const MARKER: &str = "pw-marker-7f3a";

    fn network_profile() -> ConnectionProfile {
        let mut p = ConnectionProfile::new(
            "pg",
            Engine::Postgres,
            ProfileAddress::Network {
                host: "db".into(),
                port: None,
            },
        );
        p.id = Uuid::nil();
        p.user = Some("app".into());
        p
    }

    fn keychain(key: &str) -> SecretRef {
        SecretRef::Keychain {
            key: SecretKey::new(key).unwrap(),
        }
    }

    fn typed(password: Option<&str>) -> ProvidedSecrets {
        ProvidedSecrets {
            password: password.map(Secret::new),
            ..ProvidedSecrets::default()
        }
    }

    fn ssh_password_settings(host: &str) -> SshSettings {
        SshSettings {
            host: host.into(),
            port: 22,
            user: "ops".into(),
            auth: SshAuthSettings::Password {
                password: keychain("pending"),
            },
            known_hosts: KnownHostsPolicy::default(),
            connect_timeout_secs: None,
            keepalive_secs: None,
            extra: oxbase_store::Extra::new(),
        }
    }

    #[test]
    fn normalize_points_refs_at_own_keys() {
        let mut p = network_profile();
        p.id = Uuid::now_v7();
        p.password = Some(keychain("connection/someone-else/password"));
        let mut ssh = ssh_password_settings("bastion");
        ssh.auth = SshAuthSettings::PrivateKey {
            key: KeyRef::File { path: "/k".into() },
            passphrase: Some(keychain("x")),
        };
        p.ssh = Some(ssh);
        normalize_secret_refs(&mut p);
        assert_eq!(
            p.secret_keys(),
            vec![
                p.secret_key(SecretSlot::Password),
                p.secret_key(SecretSlot::SshKeyPassphrase)
            ]
        );
    }

    #[tokio::test]
    async fn save_stores_keychain_secrets_and_never_inlines_them() {
        let repo = MemoryProfileRepository::new();
        let store = MemorySecretStore::new();
        let mut p = network_profile();
        p.password = Some(keychain("pending"));

        let saved = save_profile(&repo, &store, p, &typed(Some(MARKER)))
            .await
            .unwrap();
        assert!(!saved.id.is_nil(), "new profiles get an id");
        let key = saved.secret_key(SecretSlot::Password);
        assert_eq!(
            saved.password,
            Some(SecretRef::Keychain { key: key.clone() })
        );
        assert_eq!(store.get(&key).await.unwrap().unwrap().expose(), MARKER);
        assert!(!saved.to_json().unwrap().contains(MARKER));
        assert!(!format!("{saved:?}").contains(MARKER));

        // Editing without retyping keeps the stored password.
        let mut renamed = saved.clone();
        renamed.name = "renamed".into();
        let again = save_profile(&repo, &store, renamed.clone(), &typed(None))
            .await
            .unwrap();
        assert_eq!(again, renamed);
        assert_eq!(store.len(), 1);

        // Switching to "ask every time" deletes the stored password.
        let mut prompt = saved.clone();
        prompt.password = Some(SecretRef::Prompt);
        save_profile(&repo, &store, prompt, &typed(Some("typed-only")))
            .await
            .unwrap();
        assert!(store.is_empty(), "prompt-only values are not stored");
    }

    #[tokio::test]
    async fn save_rejects_keychain_ref_without_a_value() {
        let repo = MemoryProfileRepository::new();
        let store = MemorySecretStore::new();
        let mut p = network_profile();
        p.password = Some(keychain("pending"));
        let err = save_profile(&repo, &store, p, &typed(Some("")))
            .await
            .unwrap_err();
        assert_eq!(err.category(), ErrorCategory::InvalidInput);
        assert!(err.to_string().contains("database password"), "{err}");
        assert_eq!(repo.list().await.unwrap(), Vec::<ConnectionProfile>::new());
    }

    #[tokio::test]
    async fn save_validates_before_touching_the_store() {
        let repo = MemoryProfileRepository::new();
        let store = MemorySecretStore::new();
        let mut p = network_profile();
        p.name = " ".into();
        p.password = Some(keychain("pending"));
        let err = save_profile(&repo, &store, p, &typed(Some(MARKER)))
            .await
            .unwrap_err();
        assert_eq!(err.category(), ErrorCategory::InvalidInput);
        assert!(store.is_empty());
    }

    /// The stored database and SSH passwords are never carried over to another destination by a
    /// save that does not retype them; the saved profile and its secrets stay as they were.
    #[tokio::test]
    async fn save_requires_retyping_sent_secrets_when_the_destination_changes() {
        let repo = MemoryProfileRepository::new();
        let store = MemorySecretStore::new();
        let mut p = network_profile();
        p.password = Some(keychain("pending"));
        p.ssh = Some(ssh_password_settings("bastion"));
        let both = ProvidedSecrets {
            password: Some(Secret::new(MARKER)),
            ssh_password: Some(Secret::new("ssh-marker")),
            ssh_key_passphrase: None,
        };
        let saved = save_profile(&repo, &store, p, &both).await.unwrap();

        let mut other_host = saved.clone();
        other_host.address = ProfileAddress::Network {
            host: "attacker.example".into(),
            port: None,
        };
        let mut weaker_tls = saved.clone();
        weaker_tls.tls.mode = oxbase_store::TlsMode::Disable;
        let mut other_user = saved.clone();
        other_user.user = Some("root".into());
        let mut other_bastion = saved.clone();
        if let Some(ssh) = &mut other_bastion.ssh {
            ssh.host = "elsewhere".into();
        }
        let mut no_ssh = saved.clone();
        no_ssh.ssh = None;

        for (moved, needs) in [
            (&other_host, "database password"),
            (&weaker_tls, "database password"),
            (&other_user, "database password"),
            (&other_bastion, "database password"),
            (&no_ssh, "database password"),
        ] {
            let err = save_profile(&repo, &store, moved.clone(), &typed(None))
                .await
                .unwrap_err();
            assert_eq!(err.category(), ErrorCategory::InvalidInput, "{err}");
            assert!(err.to_string().contains(needs), "{err}");
            // Retyping only the database password is not enough while SSH still sends one.
            if moved.ssh.is_some() {
                let err = save_profile(&repo, &store, moved.clone(), &typed(Some("new")))
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("SSH password"), "{err}");
            }
            assert_eq!(repo.get(saved.id).await.unwrap().as_ref(), Some(&saved));
            assert_eq!(
                store
                    .get(&saved.secret_key(SecretSlot::Password))
                    .await
                    .unwrap()
                    .unwrap()
                    .expose(),
                MARKER
            );
        }

        // Retyped values are stored for the new destination and replace the old ones.
        let retyped = ProvidedSecrets {
            password: Some(Secret::new("new-db")),
            ssh_password: Some(Secret::new("new-ssh")),
            ssh_key_passphrase: None,
        };
        let moved = save_profile(&repo, &store, other_host.clone(), &retyped)
            .await
            .unwrap();
        for (slot, value) in [
            (SecretSlot::Password, "new-db"),
            (SecretSlot::SshPassword, "new-ssh"),
        ] {
            let stored = store.get(&moved.secret_key(slot)).await.unwrap().unwrap();
            assert_eq!(stored.expose(), value);
        }
        // Moving to "ask every time" needs nothing typed.
        let mut prompt = saved.clone();
        prompt.address = ProfileAddress::Network {
            host: "third".into(),
            port: None,
        };
        prompt.password = Some(SecretRef::Prompt);
        prompt.ssh = None;
        save_profile(&repo, &store, prompt, &typed(None))
            .await
            .unwrap();
        assert!(store.is_empty(), "the old secrets are removed");
    }

    /// A value left under a profile id (for example after a delete whose keychain cleanup failed)
    /// is never adopted by a new profile with that id.
    #[tokio::test]
    async fn new_profiles_do_not_inherit_leftover_passwords() {
        let repo = MemoryProfileRepository::new();
        let store = MemorySecretStore::new();
        let mut p = network_profile();
        p.id = Uuid::now_v7();
        p.password = Some(keychain("pending"));
        store
            .set(&p.secret_key(SecretSlot::Password), &Secret::new(MARKER))
            .await
            .unwrap();
        let err = save_profile(&repo, &store, p, &typed(None))
            .await
            .unwrap_err();
        assert_eq!(err.category(), ErrorCategory::InvalidInput);
        assert_eq!(repo.list().await.unwrap(), Vec::<ConnectionProfile>::new());
    }

    /// A repository whose `save` fails (e.g. disk full), delegating everything else.
    #[derive(Debug, Default)]
    struct FailingSave(MemoryProfileRepository);

    #[async_trait::async_trait]
    impl ProfileRepository for FailingSave {
        async fn list(&self) -> Result<Vec<ConnectionProfile>> {
            self.0.list().await
        }
        async fn get(&self, id: Uuid) -> Result<Option<ConnectionProfile>> {
            self.0.get(id).await
        }
        async fn save(&self, _profile: &ConnectionProfile) -> Result<()> {
            Err(std::io::Error::other("no space left on device").into())
        }
        async fn delete(&self, id: Uuid) -> Result<bool> {
            self.0.delete(id).await
        }
    }

    /// A secret store whose `delete` fails, delegating everything else.
    #[derive(Debug, Default)]
    struct FailingDelete(MemorySecretStore);

    #[async_trait::async_trait]
    impl SecretStore for FailingDelete {
        async fn get(&self, key: &SecretKey) -> Result<Option<Secret>> {
            self.0.get(key).await
        }
        async fn set(&self, key: &SecretKey, value: &Secret) -> Result<()> {
            self.0.set(key, value).await
        }
        async fn delete(&self, _key: &SecretKey) -> Result<bool> {
            Err(std::io::Error::other("keychain locked").into())
        }
    }

    #[test]
    fn own_keys_are_the_profile_namespace_only() {
        let id = Uuid::now_v7();
        let own = SecretKey::for_profile(id, SecretSlot::Password);
        assert!(is_own_key(id, &own));
        assert!(!is_own_key(Uuid::now_v7(), &own));
        assert!(!is_own_key(
            id,
            &SecretKey::new("shared/db-password").unwrap()
        ));
        let glued = SecretKey::new(format!("connection/{id}x/password")).unwrap();
        assert!(!is_own_key(id, &glued));
    }

    #[tokio::test]
    async fn failed_save_of_a_new_profile_leaves_no_secrets() {
        let repo = FailingSave::default();
        let store = MemorySecretStore::new();
        let mut p = network_profile();
        p.password = Some(keychain("pending"));
        let err = save_profile(&repo, &store, p, &typed(Some(MARKER)))
            .await
            .unwrap_err();
        assert_eq!(err.category(), ErrorCategory::Io);
        assert!(store.is_empty(), "orphaned secrets were left behind");
    }

    #[tokio::test]
    async fn saving_never_deletes_keys_outside_the_profile_namespace() {
        let repo = MemoryProfileRepository::new();
        let store = MemorySecretStore::new();
        let shared = SecretKey::new("shared/db-password").unwrap();
        store.set(&shared, &Secret::new(MARKER)).await.unwrap();
        // A hand-edited profile that points at a shared key.
        let mut p = network_profile();
        p.id = Uuid::now_v7();
        p.password = Some(SecretRef::Keychain {
            key: shared.clone(),
        });
        repo.save(&p).await.unwrap();

        let mut edited = p.clone();
        edited.password = Some(SecretRef::Prompt);
        save_profile(&repo, &store, edited, &typed(None))
            .await
            .unwrap();
        assert!(store.get(&shared).await.unwrap().is_some());

        assert!(delete_profile(&repo, &store, p.id).await.unwrap());
        repo.save(&p).await.unwrap();
        assert!(delete_profile(&repo, &store, p.id).await.unwrap());
        assert!(store.get(&shared).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn delete_succeeds_when_the_keychain_fails() {
        let repo = MemoryProfileRepository::new();
        let store = FailingDelete::default();
        let mut p = network_profile();
        p.password = Some(keychain("pending"));
        let saved = save_profile(&repo, &store, p, &typed(Some(MARKER)))
            .await
            .unwrap();
        assert!(delete_profile(&repo, &store, saved.id).await.unwrap());
        assert!(repo.get(saved.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn changed_destinations_do_not_get_stored_secrets() {
        let mut saved = network_profile();
        saved.id = Uuid::now_v7();
        saved.password = Some(keychain("pending"));
        saved.ssh = Some(ssh_password_settings("bastion"));
        normalize_secret_refs(&mut saved);
        assert!(same_destination(&saved, &saved));

        let mut renamed = saved.clone();
        renamed.name = "renamed".into();
        renamed.database = Some("other".into());
        assert!(
            same_destination(&saved, &renamed),
            "not part of the destination"
        );

        let mut other_host = saved.clone();
        other_host.address = ProfileAddress::Network {
            host: "attacker.example".into(),
            port: None,
        };
        let mut weaker_tls = saved.clone();
        weaker_tls.tls.mode = oxbase_store::TlsMode::Disable;
        let mut other_bastion = saved.clone();
        if let Some(ssh) = &mut other_bastion.ssh {
            ssh.host = "elsewhere".into();
        }
        let mut other_user = saved.clone();
        other_user.user = Some("root".into());
        for changed in [&other_host, &weaker_tls, &other_bastion, &other_user] {
            assert!(!same_destination(&saved, changed));
        }

        let repo = MemoryProfileRepository::new();
        repo.save(&saved).await.unwrap();
        let mut same = renamed.clone();
        restrict_to_saved_destination(&repo, &mut same)
            .await
            .unwrap();
        assert_eq!(same, renamed, "same destination keeps keychain refs");

        restrict_to_saved_destination(&repo, &mut other_host)
            .await
            .unwrap();
        assert_eq!(other_host.password, Some(SecretRef::Prompt));
        assert_eq!(
            other_host.ssh.map(|ssh| ssh.auth),
            Some(SshAuthSettings::Password {
                password: SecretRef::Prompt
            })
        );
    }

    /// Whether the OS keychain tests are enabled (`OXBASE_KEYCHAIN_TESTS=1`). They also run only
    /// with `--ignored`, so `--include-ignored` runs without a keychain (the Docker fixture job)
    /// skip them.
    fn keychain_tests_enabled() -> bool {
        let on = std::env::var("OXBASE_KEYCHAIN_TESTS").is_ok_and(|v| v == "1");
        if !on {
            eprintln!("skipped: set OXBASE_KEYCHAIN_TESTS=1 to use the OS keychain");
        }
        on
    }

    /// The connection manager backed by the real OS keychain: a profile saved through
    /// [`save_profile`] into [`oxbase_store::KeyringSecretStore`] resolves and opens with the
    /// stored database and SSH passwords, an edit to another host must retype them, and
    /// [`delete_profile`] removes them from the keychain again.
    /// `scripts/keychain-tests.sh` (or `OXBASE_KEYCHAIN_TESTS=1 cargo test -p oxbase-services --
    /// --ignored keychain` in a desktop session).
    #[tokio::test]
    #[ignore = "needs an OS keychain (desktop session) and OXBASE_KEYCHAIN_TESTS=1"]
    async fn keychain_backed_profile_round_trip() {
        use std::sync::Arc;

        use oxbase_driver::mock::MockDriver;
        use oxbase_net::ssh::RusshTunnelProvider;
        use oxbase_store::KeyringSecretStore;

        use crate::{CancellationToken, ConnectionManager, DriverRegistry};

        if !keychain_tests_enabled() {
            return;
        }
        let store = Arc::new(KeyringSecretStore::with_service(format!(
            "{}.test-{}",
            KeyringSecretStore::DEFAULT_SERVICE,
            Uuid::new_v4().simple()
        )));
        let repo = MemoryProfileRepository::new();
        let mut drivers = DriverRegistry::new();
        drivers.register(Arc::new(MockDriver::new(Engine::Postgres)));
        let manager =
            ConnectionManager::new(drivers, store.clone(), Arc::new(RusshTunnelProvider::new()));

        let mut p = network_profile();
        p.password = Some(keychain("pending"));
        let saved = save_profile(&repo, store.as_ref(), p, &typed(Some(MARKER)))
            .await
            .unwrap();
        let resolved = manager
            .resolve(&saved, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            resolved.params.password.as_ref().map(Secret::expose),
            Some(MARKER)
        );
        let session = manager
            .open(&saved, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .unwrap();
        manager.close(session.id()).await.unwrap();

        let mut moved = saved.clone();
        moved.address = ProfileAddress::Network {
            host: "attacker.example".into(),
            port: None,
        };
        let err = save_profile(&repo, store.as_ref(), moved, &typed(None))
            .await
            .unwrap_err();
        assert_eq!(err.category(), ErrorCategory::InvalidInput, "{err}");

        // A long value (an RSA-4096 key is about 3.3 KB) as the key passphrase slot exercises the
        // Windows chunking through the same path.
        let mut with_key = saved.clone();
        let long = "p".repeat(3500);
        let mut ssh = ssh_password_settings("bastion");
        ssh.auth = SshAuthSettings::PrivateKey {
            key: KeyRef::File { path: "/k".into() },
            passphrase: Some(keychain("pending")),
        };
        with_key.ssh = Some(ssh);
        let passphrase = ProvidedSecrets {
            password: Some(Secret::new(MARKER)),
            ssh_password: None,
            ssh_key_passphrase: Some(Secret::new(long.clone())),
        };
        let with_key = save_profile(&repo, store.as_ref(), with_key, &passphrase)
            .await
            .unwrap();
        let stored = store
            .get(&with_key.secret_key(SecretSlot::SshKeyPassphrase))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.expose(), long);

        assert!(
            delete_profile(&repo, store.as_ref(), saved.id)
                .await
                .unwrap()
        );
        for slot in [SecretSlot::Password, SecretSlot::SshKeyPassphrase] {
            assert_eq!(store.get(&saved.secret_key(slot)).await.unwrap(), None);
        }
    }

    #[tokio::test]
    async fn delete_removes_profile_and_secrets() {
        let repo = MemoryProfileRepository::new();
        let store = MemorySecretStore::new();
        let mut p = network_profile();
        p.password = Some(keychain("pending"));
        let saved = save_profile(&repo, &store, p, &typed(Some(MARKER)))
            .await
            .unwrap();
        assert!(delete_profile(&repo, &store, saved.id).await.unwrap());
        assert!(store.is_empty());
        assert!(repo.get(saved.id).await.unwrap().is_none());
        assert!(!delete_profile(&repo, &store, saved.id).await.unwrap());
    }
}
