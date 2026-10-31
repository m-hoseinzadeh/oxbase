//! The error shape every command returns to the UI: `{ "category": ..., "message": ... }`.
//!
//! `category` is [`ErrorCategory`] in snake_case (`connection`, `auth`, `timeout`, ...), so the UI
//! can react without parsing text. `message` is the core's human-readable message *without* the
//! category prefix that `Error`'s `Display` adds (the UI shows the category separately).
//!
//! Secrets: core errors never contain secrets (crate contracts). As a second line of defence,
//! [`CommandError::redact`] masks any secret the command received that still shows up in a
//! message. A panic inside the core becomes a generic `internal` error; its payload is never
//! forwarded, because it is not guaranteed to be free of secrets.

use oxbase_types::{Error, ErrorCategory, Secret};
use serde::Serialize;

/// Secrets shorter than this are not masked in messages: masking one or two characters would
/// garble every word that contains them while hiding nothing useful.
const MIN_REDACT_LEN: usize = 3;

/// What replaces a secret found in a message.
const REDACTED: &str = "***";

/// Serializable command error: `{ category, message }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandError {
    pub category: ErrorCategory,
    pub message: String,
}

impl CommandError {
    pub fn new(category: ErrorCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
        }
    }

    /// A core task panicked or was aborted. The panic payload is deliberately dropped.
    pub fn aborted() -> Self {
        Self::new(
            ErrorCategory::Internal,
            "the operation stopped unexpectedly (internal error)",
        )
    }

    /// Masks every non-trivial secret in `secrets` that appears in the message.
    #[must_use]
    pub fn redact<'a>(mut self, secrets: impl IntoIterator<Item = &'a Secret>) -> Self {
        for secret in secrets {
            let text = secret.expose();
            if text.len() >= MIN_REDACT_LEN && self.message.contains(text) {
                self.message = self.message.replace(text, REDACTED);
            }
        }
        self
    }
}

impl From<Error> for CommandError {
    fn from(error: Error) -> Self {
        let category = error.category();
        let message = match &error {
            Error::Connection { message }
            | Error::Auth { message }
            | Error::Timeout { message }
            | Error::Unsupported { message }
            | Error::NotFound { message }
            | Error::InvalidInput { message }
            | Error::Internal { message }
            | Error::Io { message, .. }
                if !message.trim().is_empty() =>
            {
                message.clone()
            }
            Error::Cancelled => "the operation was cancelled".to_owned(),
            // `Sql` keeps its code ("SQL error [42P01]: ..."); empty messages fall back to the
            // full display text so the UI never shows a blank error.
            _ => error.to_string(),
        };
        Self { category, message }
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.category, self.message)
    }
}

impl std::error::Error for CommandError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_category_maps_to_its_snake_case_name() {
        let cases = [
            (Error::connection("refused"), "connection", "refused"),
            (Error::auth("bad password"), "auth", "bad password"),
            (Error::timeout("after 10 s"), "timeout", "after 10 s"),
            (Error::Cancelled, "cancelled", "the operation was cancelled"),
            (Error::unsupported("no driver"), "unsupported", "no driver"),
            (
                Error::sql(Some("42P01"), "relation does not exist"),
                "sql",
                "SQL error [42P01]: relation does not exist",
            ),
            (Error::not_found("no profile"), "not_found", "no profile"),
            (
                Error::invalid_input("bad port"),
                "invalid_input",
                "bad port",
            ),
            (
                Error::Io {
                    kind: "not found".into(),
                    message: "no such file".into(),
                },
                "io",
                "no such file",
            ),
            (
                Error::internal("lock poisoned"),
                "internal",
                "lock poisoned",
            ),
        ];
        for (error, category, message) in cases {
            let json = serde_json::to_value(CommandError::from(error)).unwrap();
            assert_eq!(json, json!({ "category": category, "message": message }));
        }
    }

    #[test]
    fn empty_messages_fall_back_to_display_text() {
        let e = CommandError::from(Error::auth("  "));
        assert_eq!(e.category, ErrorCategory::Auth);
        assert_eq!(e.message, "authentication failed:   ");
        let e = CommandError::from(Error::sql(None::<String>, "boom"));
        assert_eq!(e.message, "SQL error: boom");
    }

    #[test]
    fn redact_masks_received_secrets_only() {
        let password = Secret::new("hunter2-marker");
        let short = Secret::new("ab");
        let e = CommandError::from(Error::auth(
            "server said: password hunter2-marker rejected for ab",
        ))
        .redact([&password, &short]);
        assert_eq!(e.message, "server said: password *** rejected for ab");
        assert!(!format!("{e:?} {e}").contains("hunter2-marker"));
    }

    #[test]
    fn aborted_is_internal_and_generic() {
        let e = CommandError::aborted();
        assert_eq!(e.category, ErrorCategory::Internal);
        assert_eq!(
            e.to_string(),
            "internal: the operation stopped unexpectedly (internal error)"
        );
    }
}
