//! Mapping `tiberius::error::Error` to [`oxbase_types::Error`] (driver stream).
//!
//! - `Error::Server(token)`: `Error::Sql { code: Some(<number>), message }`; the message adds
//!   `procedure` and `line` when the server sends them. Number 18456 (login failed) and 18452,
//!   18486, 18487, 18488 (untrusted / locked / expired logins) are `Error::Auth`; 4060 (cannot
//!   open the database) is `Error::Sql` with its number.
//! - I/O errors before or during login: `Error::Connection` naming the server endpoint (never
//!   the password); timeouts `Error::Timeout`; TLS failures `Error::Connection` with the rustls
//!   reason (`invalid peer certificate: ...`).
//! - A protocol error after login (desynchronised stream): `Error::Connection`, and the
//!   connection is closed.
//!
//! No message may contain the password or a client key, whatever tiberius puts in its errors.

use std::fmt::Write as _;

use oxbase_types::Error;
use tiberius::error::{Error as TdsError, IoErrorKind, TokenError};

/// Login failures that mean the credentials or the login itself were refused.
pub(crate) const AUTH_ERRORS: [u32; 5] = [18456, 18452, 18486, 18487, 18488];

/// The server's message with the procedure and line when it sent them.
pub(crate) fn server_message(e: &TokenError) -> String {
    let mut message = e.message().to_owned();
    match (e.procedure(), e.line()) {
        ("", 0) => {}
        ("", line) => {
            let _ = write!(message, " (line {line})");
        }
        (procedure, line) => {
            let _ = write!(message, " (procedure {procedure}, line {line})");
        }
    }
    message
}

/// An error raised by tiberius, and whether the session is unusable after it (I/O and
/// protocol failures leave the stream out of step; server errors do not).
pub(crate) struct Mapped {
    pub error: Error,
    pub fatal: bool,
}

/// Maps an error raised on an open session.
pub(crate) fn map_error(e: TdsError) -> Mapped {
    let (error, fatal) = match e {
        TdsError::Server(t) => {
            if AUTH_ERRORS.contains(&t.code()) {
                (Error::auth(server_message(&t)), false)
            } else {
                (
                    Error::sql(Some(t.code().to_string()), server_message(&t)),
                    false,
                )
            }
        }
        TdsError::Io { kind, message } if kind == IoErrorKind::TimedOut => {
            (Error::timeout(message), true)
        }
        TdsError::Io { message, .. } => (Error::connection(format!("I/O: {message}")), true),
        TdsError::Protocol(m) => (Error::connection(format!("protocol error: {m}")), true),
        TdsError::Tls(m) => (Error::connection(format!("TLS: {m}")), true),
        TdsError::Routing { host, port } => (
            Error::unsupported(format!(
                "the server redirects the connection to {host}:{port}, which oxbase does not \
                 follow yet"
            )),
            true,
        ),
        TdsError::BulkInput(m) => (Error::invalid_input(m.into_owned()), false),
        other => (
            Error::sql(None::<String>, format!("cannot convert a value: {other}")),
            false,
        ),
    };
    Mapped { error, fatal }
}

/// Maps an error raised while connecting to `server` (the endpoint shown to the user).
pub(crate) fn map_connect_error(e: TdsError, server: &str) -> Error {
    with_server(map_error(e).error, server)
}

/// Prefixes connection and authentication errors with the server endpoint.
pub(crate) fn with_server(e: Error, server: &str) -> Error {
    match e {
        Error::Connection { message } => {
            Error::connection(format!("cannot connect to {server}: {message}"))
        }
        Error::Auth { message } => Error::auth(format!("{server}: {message}")),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_errors_are_fatal_connection_errors() {
        let m = map_error(TdsError::Io {
            kind: IoErrorKind::ConnectionReset,
            message: "reset".into(),
        });
        assert!(m.fatal);
        assert!(matches!(m.error, Error::Connection { .. }));
        let m = map_error(TdsError::Io {
            kind: IoErrorKind::TimedOut,
            message: "slow".into(),
        });
        assert!(matches!(m.error, Error::Timeout { .. }));
        let m = map_error(TdsError::Protocol("desync".into()));
        assert!(m.fatal);
        let m = map_error(TdsError::Utf16);
        assert!(!m.fatal);
        assert!(matches!(m.error, Error::Sql { code: None, .. }));
        let e = with_server(
            map_error(TdsError::Tls("bad cert".into())).error,
            "db.example:1433",
        );
        assert!(e.to_string().contains("db.example:1433"), "{e}");
        assert!(matches!(
            with_server(Error::auth("no"), "h:1"),
            Error::Auth { message } if message == "h:1: no"
        ));
    }
}
