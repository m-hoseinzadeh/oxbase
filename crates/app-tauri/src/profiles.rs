//! Secret values typed in the UI. The rules for saving, testing and deleting profiles with their
//! keychain secrets live in [`oxbase_services::profiles`] (shared with `oxbase-cli`); this module
//! only deserializes the UI's input and keeps it out of `Debug` output.

use std::fmt;

use oxbase_services::ProvidedSecrets;
use oxbase_types::Secret;
use serde::{Deserialize, Deserializer};

/// A secret received from the UI. Deserializes from a JSON string; `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretArg(pub Secret);

impl<'de> Deserialize<'de> for SecretArg {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|s| Self(Secret::new(s)))
    }
}

impl fmt::Debug for SecretArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Secret values typed in the UI for one call. Empty strings count as "not given".
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecretInput {
    pub password: Option<SecretArg>,
    pub ssh_password: Option<SecretArg>,
    pub ssh_key_passphrase: Option<SecretArg>,
}

impl SecretInput {
    fn pick(value: Option<&SecretArg>) -> Option<Secret> {
        value.map(|v| v.0.clone()).filter(|s| !s.is_empty())
    }

    /// The non-empty values as [`ProvidedSecrets`] for the connection manager.
    pub fn provided(&self) -> ProvidedSecrets {
        ProvidedSecrets {
            password: Self::pick(self.password.as_ref()),
            ssh_password: Self::pick(self.ssh_password.as_ref()),
            ssh_key_passphrase: Self::pick(self.ssh_key_passphrase.as_ref()),
        }
    }

    /// Every non-empty value (for masking error messages).
    pub fn secrets(&self) -> Vec<Secret> {
        [&self.password, &self.ssh_password, &self.ssh_key_passphrase]
            .into_iter()
            .filter_map(|v| Self::pick(v.as_ref()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = "pw-marker-7f3a";

    #[test]
    fn secret_input_deserializes_and_redacts() {
        let input: SecretInput = serde_json::from_value(serde_json::json!({
            "password": MARKER, "ssh_password": "", "ssh_key_passphrase": null
        }))
        .unwrap();
        assert!(!format!("{input:?}").contains(MARKER));
        let provided = input.provided();
        assert_eq!(provided.password.unwrap().expose(), MARKER);
        assert_eq!(provided.ssh_password, None, "empty means not given");
        assert_eq!(input.secrets().len(), 1);
        assert!(serde_json::from_value::<SecretInput>(serde_json::json!({"pin": "1"})).is_err());
    }
}
