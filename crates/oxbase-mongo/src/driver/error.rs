//! `mongodb` errors → [`oxbase_types::Error`] (driver module docs, *Auth*, *Cancel*).
//!
//! - authentication failures (the handshake's `Authentication`, server code 18) → `Auth`;
//! - `Interrupted` (11601, a `killOp`) → `Cancelled`; `MaxTimeMSExpired` (50) and
//!   `ExceededTimeLimit` (262) → `Timeout`;
//! - other server errors → `Sql { code: <codeName> }`;
//! - network, proxy, DNS and server-selection failures → `Connection`;
//! - bad arguments and BSON errors → `InvalidInput`.
//!
//! Messages never carry the password: the server's texts do not include it, and every message
//! is passed through [`Redactor`] anyway.

use mongodb::error::{Error as MongoError, ErrorKind, WriteFailure};
use oxbase_types::{Error, Secret};

/// Server error codes the mapping treats specially.
const AUTHENTICATION_FAILED: i32 = 18;
const MAX_TIME_MS_EXPIRED: i32 = 50;
const EXCEEDED_TIME_LIMIT: i32 = 262;
const INTERRUPTED: i32 = 11601;

/// Removes a password from text that might echo it (shortest redacted secret: 4 characters,
/// so a one-letter password does not garble every message).
#[derive(Clone, Default)]
pub(crate) struct Redactor {
    secret: Option<Secret>,
}

impl Redactor {
    pub(crate) fn new(secret: Option<&Secret>) -> Self {
        Self {
            secret: secret.filter(|s| s.len() >= 4).cloned(),
        }
    }

    pub(crate) fn apply(&self, text: &str) -> String {
        match &self.secret {
            Some(secret) if text.contains(secret.expose()) => text.replace(secret.expose(), "***"),
            _ => text.to_owned(),
        }
    }
}

/// The oxbase error for `e`.
pub(crate) fn map_error(e: &MongoError, redactor: &Redactor) -> Error {
    let text = |m: &str| redactor.apply(m);
    match e.kind.as_ref() {
        ErrorKind::Authentication { message, .. } => Error::auth(text(message)),
        ErrorKind::Command(c) => server_error(c.code, &c.code_name, &text(&c.message)),
        ErrorKind::Write(WriteFailure::WriteError(w)) => server_error(
            w.code,
            w.code_name.as_deref().unwrap_or_default(),
            &text(&w.message),
        ),
        ErrorKind::Write(WriteFailure::WriteConcernError(w)) => {
            server_error(w.code, &w.code_name, &text(&w.message))
        }
        ErrorKind::ServerSelection { message, .. }
        | ErrorKind::ProxyConnect { message, .. }
        | ErrorKind::DnsResolve { message, .. }
        | ErrorKind::ConnectionPoolCleared { message, .. } => Error::connection(text(message)),
        ErrorKind::Io(io) => Error::connection(text(&io.to_string())),
        ErrorKind::Shutdown => Error::connection("the MongoDB connection is closed"),
        ErrorKind::InvalidArgument { message, .. }
        | ErrorKind::InvalidTlsConfig { message, .. } => Error::invalid_input(text(message)),
        ErrorKind::BsonDeserialization(err) => Error::invalid_input(text(&err.to_string())),
        ErrorKind::BsonSerialization(err) => Error::invalid_input(text(&err.to_string())),
        ErrorKind::Bson(err) => Error::invalid_input(text(&err.to_string())),
        ErrorKind::IncompatibleServer { message, .. } => Error::unsupported(text(message)),
        ErrorKind::SessionsNotSupported => {
            Error::unsupported("this MongoDB deployment does not support sessions")
        }
        ErrorKind::InvalidResponse { message, .. } | ErrorKind::Internal { message, .. } => {
            Error::internal(text(message))
        }
        _ => Error::sql(None::<String>, text(&e.to_string())),
    }
}

fn server_error(code: i32, code_name: &str, message: &str) -> Error {
    match code {
        AUTHENTICATION_FAILED => Error::auth(message),
        INTERRUPTED => Error::Cancelled,
        MAX_TIME_MS_EXPIRED | EXCEEDED_TIME_LIMIT => Error::timeout(message),
        _ => {
            let code = if code_name.is_empty() {
                code.to_string()
            } else {
                code_name.to_owned()
            };
            Error::sql(Some(code), message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_codes_map_to_categories() {
        assert!(matches!(
            server_error(18, "AuthenticationFailed", "nope"),
            Error::Auth { .. }
        ));
        assert!(server_error(11601, "Interrupted", "killed").is_cancelled());
        assert!(matches!(
            server_error(50, "MaxTimeMSExpired", "slow"),
            Error::Timeout { .. }
        ));
        let other = server_error(26, "NamespaceNotFound", "ns not found");
        assert_eq!(other.sql_code(), Some("NamespaceNotFound"));
        assert_eq!(server_error(9999, "", "x").sql_code(), Some("9999"));
    }

    #[test]
    fn redactor_hides_the_password() {
        let secret = Secret::new("hunter22");
        let r = Redactor::new(Some(&secret));
        assert_eq!(r.apply("bad hunter22 here"), "bad *** here");
        assert_eq!(r.apply("clean"), "clean");
        let short = Secret::new("a");
        assert_eq!(Redactor::new(Some(&short)).apply("a cat"), "a cat");
        assert_eq!(Redactor::default().apply("x"), "x");
    }
}
