//! The single error type shared by every oxbase crate.
//!
//! Errors are grouped into a small set of categories ([`ErrorCategory`]) so callers (the UI, the
//! job runner, retry logic) can react without knowing which engine produced them. The type is
//! `Clone` and serializable so it can be cached, broadcast to several listeners and sent across
//! the app boundary unchanged.

use std::fmt;
use std::io;

use serde::{Deserialize, Serialize};

/// Result alias used across oxbase.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The one error type used by drivers, dialects and services.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "category", rename_all = "snake_case")]
pub enum Error {
    /// The network connection could not be made or was lost.
    #[error("connection error: {message}")]
    Connection { message: String },

    /// The server rejected the credentials.
    #[error("authentication failed: {message}")]
    Auth { message: String },

    /// An operation took longer than its deadline.
    #[error("timed out: {message}")]
    Timeout { message: String },

    /// The operation was cancelled through its `CancellationToken` or a server-side cancel.
    #[error("cancelled")]
    Cancelled,

    /// The engine, server version or this build does not support the requested operation.
    #[error("unsupported: {message}")]
    Unsupported { message: String },

    /// The server reported an error for a statement.
    #[error("{}", fmt_sql(code.as_deref(), message))]
    Sql {
        /// Engine error code (SQLSTATE for PostgreSQL, error number for MySQL/SQL Server, ...).
        code: Option<String>,
        /// The server's message text.
        message: String,
    },

    /// The requested object or row does not exist.
    #[error("not found: {message}")]
    NotFound { message: String },

    /// The caller passed something malformed (bad parameters, unparsable version, mismatched key).
    #[error("invalid input: {message}")]
    InvalidInput { message: String },

    /// A local I/O failure (files, sockets before a session exists, pipes).
    #[error("I/O error ({kind}): {message}")]
    Io {
        /// `std::io::ErrorKind` rendered as text (the enum itself is not serializable).
        kind: String,
        message: String,
    },

    /// A bug or broken invariant inside oxbase.
    #[error("internal error: {message}")]
    Internal { message: String },
}

fn fmt_sql(code: Option<&str>, message: &str) -> String {
    match code {
        Some(code) => format!("SQL error [{code}]: {message}"),
        None => format!("SQL error: {message}"),
    }
}

/// Coarse classification of an [`Error`], stable across engines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    Connection,
    Auth,
    Timeout,
    Cancelled,
    Unsupported,
    Sql,
    NotFound,
    InvalidInput,
    Io,
    Internal,
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Connection => "connection",
            Self::Auth => "auth",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Unsupported => "unsupported",
            Self::Sql => "sql",
            Self::NotFound => "not_found",
            Self::InvalidInput => "invalid_input",
            Self::Io => "io",
            Self::Internal => "internal",
        };
        f.write_str(s)
    }
}

impl Error {
    pub fn connection(message: impl Into<String>) -> Self {
        Self::Connection {
            message: message.into(),
        }
    }

    pub fn auth(message: impl Into<String>) -> Self {
        Self::Auth {
            message: message.into(),
        }
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::Timeout {
            message: message.into(),
        }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported {
            message: message.into(),
        }
    }

    pub fn sql(code: Option<impl Into<String>>, message: impl Into<String>) -> Self {
        Self::Sql {
            code: code.map(Into::into),
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound {
            message: message.into(),
        }
    }

    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::InvalidInput {
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }

    /// The category of this error.
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::Connection { .. } => ErrorCategory::Connection,
            Self::Auth { .. } => ErrorCategory::Auth,
            Self::Timeout { .. } => ErrorCategory::Timeout,
            Self::Cancelled => ErrorCategory::Cancelled,
            Self::Unsupported { .. } => ErrorCategory::Unsupported,
            Self::Sql { .. } => ErrorCategory::Sql,
            Self::NotFound { .. } => ErrorCategory::NotFound,
            Self::InvalidInput { .. } => ErrorCategory::InvalidInput,
            Self::Io { .. } => ErrorCategory::Io,
            Self::Internal { .. } => ErrorCategory::Internal,
        }
    }

    /// `true` for [`Error::Cancelled`].
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// `true` when retrying the same operation later may succeed (lost connection, timeout).
    /// Never true for SQL, auth or input errors.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Connection { .. } | Self::Timeout { .. } | Self::Io { .. }
        )
    }

    /// The engine error code, when this is an [`Error::Sql`] that carries one.
    pub fn sql_code(&self) -> Option<&str> {
        match self {
            Self::Sql { code, .. } => code.as_deref(),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::TimedOut => Self::timeout(e.to_string()),
            kind => Self::Io {
                kind: kind.to_string(),
                message: e.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn categories() {
        assert_eq!(Error::connection("x").category(), ErrorCategory::Connection);
        assert_eq!(Error::auth("x").category(), ErrorCategory::Auth);
        assert_eq!(Error::timeout("x").category(), ErrorCategory::Timeout);
        assert_eq!(Error::Cancelled.category(), ErrorCategory::Cancelled);
        assert_eq!(
            Error::unsupported("x").category(),
            ErrorCategory::Unsupported
        );
        assert_eq!(
            Error::sql(Some("42P01"), "relation does not exist").category(),
            ErrorCategory::Sql
        );
        assert_eq!(Error::internal("x").category(), ErrorCategory::Internal);
        assert!(Error::Cancelled.is_cancelled());
        assert!(Error::connection("reset").is_transient());
        assert!(!Error::sql(None::<String>, "syntax").is_transient());
    }

    #[test]
    fn display() {
        assert_eq!(
            Error::sql(Some("42P01"), "relation \"t\" does not exist").to_string(),
            "SQL error [42P01]: relation \"t\" does not exist"
        );
        assert_eq!(
            Error::sql(None::<String>, "boom").to_string(),
            "SQL error: boom"
        );
        assert_eq!(Error::Cancelled.to_string(), "cancelled");
        assert_eq!(Error::sql(Some("1146"), "x").sql_code(), Some("1146"));
    }

    #[test]
    fn from_io() {
        let e: Error = io::Error::new(io::ErrorKind::NotFound, "no such file").into();
        assert_eq!(e.category(), ErrorCategory::Io);
        let e: Error = io::Error::new(io::ErrorKind::TimedOut, "slow").into();
        assert_eq!(e.category(), ErrorCategory::Timeout);
    }

    #[test]
    fn serde_round_trip() {
        let e = Error::sql(Some("23505"), "duplicate key");
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(
            json,
            r#"{"category":"sql","code":"23505","message":"duplicate key"}"#
        );
        let back: Error = serde_json::from_str(&json).unwrap();
        assert_eq!(back, e);
        let json = serde_json::to_string(&Error::Cancelled).unwrap();
        assert_eq!(json, r#"{"category":"cancelled"}"#);
    }
}
