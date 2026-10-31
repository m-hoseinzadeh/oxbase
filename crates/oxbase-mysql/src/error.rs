//! Mapping `mysql_async` errors to [`oxbase_types::Error`].
//!
//! Messages come from the server or from `mysql_async`'s `Display` output, which never includes
//! the password, key material or connection options. `mysql_async::Opts` and `Conn` *do* print
//! the password in their `Debug` output, so this crate never formats them with `{:?}`.

use std::io;

use mysql_async::{DriverError, IoError};
use oxbase_types::Error;

/// `ER_QUERY_INTERRUPTED` (MySQL and MariaDB): the statement was stopped by `KILL QUERY`.
pub(crate) const ER_QUERY_INTERRUPTED: u16 = 1317;
/// `ER_NO_SUCH_THREAD`: `KILL` named a connection that no longer exists.
pub(crate) const ER_NO_SUCH_THREAD: u16 = 1094;

/// Server error numbers that mean the credentials (or the account's access) were rejected.
const AUTH_ERRORS: [u16; 6] = [
    1044, // ER_DBACCESS_DENIED_ERROR
    1045, // ER_ACCESS_DENIED_ERROR
    1251, // ER_NOT_SUPPORTED_AUTH_MODE
    1524, // ER_PLUGIN_IS_NOT_LOADED
    1698, // ER_ACCESS_DENIED_NO_PASSWORD_ERROR
    1862, // ER_MUST_CHANGE_PASSWORD_LOGIN
];

/// Statement deadlines: MySQL `max_execution_time` (3024), MariaDB `max_statement_time` (1969).
const TIMEOUT_ERRORS: [u16; 2] = [3024, 1969];

/// `true` when `e` is the server error `code`.
pub(crate) fn is_server_error(e: &mysql_async::Error, code: u16) -> bool {
    matches!(e, mysql_async::Error::Server(s) if s.code == code)
}

/// `true` when the server does not offer TLS (the `Prefer` fallback trigger).
pub(crate) fn is_no_tls_offered(e: &mysql_async::Error) -> bool {
    matches!(
        e,
        mysql_async::Error::Driver(DriverError::NoClientSslFlagFromServer)
    )
}

/// Maps an error raised while running statements on an open session.
pub(crate) fn map_error(e: mysql_async::Error) -> Error {
    match e {
        mysql_async::Error::Server(s) => {
            if s.code == ER_QUERY_INTERRUPTED {
                Error::Cancelled
            } else if AUTH_ERRORS.contains(&s.code) {
                Error::auth(s.message)
            } else if TIMEOUT_ERRORS.contains(&s.code) {
                Error::timeout(s.message)
            } else {
                Error::sql(Some(s.code.to_string()), s.message)
            }
        }
        mysql_async::Error::Io(IoError::Io(io)) => map_io(&io),
        mysql_async::Error::Io(IoError::Tls(tls)) => Error::connection(format!("TLS: {tls}")),
        mysql_async::Error::Driver(d) => match d {
            DriverError::StmtParamsMismatch { .. }
            | DriverError::Params(_)
            | DriverError::MixedParams
            | DriverError::StmtParamsNumberExceedsLimit { .. } => {
                Error::invalid_input(d.to_string())
            }
            DriverError::NoClientSslFlagFromServer => {
                Error::connection("the server does not support TLS")
            }
            other => Error::connection(other.to_string()),
        },
        mysql_async::Error::Url(u) => Error::invalid_input(u.to_string()),
        mysql_async::Error::Other(o) => Error::internal(o.to_string()),
    }
}

fn map_io(io: &io::Error) -> Error {
    if io.kind() == io::ErrorKind::TimedOut {
        Error::timeout(io.to_string())
    } else {
        Error::connection(format!("I/O: {io}"))
    }
}

/// Maps an error raised while connecting to `server` (the endpoint shown to the user).
pub(crate) fn map_connect_error(e: mysql_async::Error, server: &str) -> Error {
    match map_error(e) {
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
    use mysql_async::ServerError;

    fn server(code: u16, message: &str) -> mysql_async::Error {
        mysql_async::Error::Server(ServerError {
            code,
            message: message.to_owned(),
            state: "HY000".to_owned(),
        })
    }

    #[test]
    fn server_errors_are_classified() {
        assert!(map_error(server(1317, "interrupted")).is_cancelled());
        assert!(matches!(
            map_error(server(1045, "Access denied")),
            Error::Auth { .. }
        ));
        assert!(matches!(
            map_error(server(3024, "max_execution_time")),
            Error::Timeout { .. }
        ));
        let e = map_error(server(1146, "Table 'x.y' doesn't exist"));
        assert_eq!(e.sql_code(), Some("1146"));
        assert!(is_server_error(&server(1094, "x"), ER_NO_SUCH_THREAD));
        assert!(!is_server_error(&server(1094, "x"), ER_QUERY_INTERRUPTED));
    }

    #[test]
    fn transport_errors_are_connection_errors() {
        let refused = mysql_async::Error::Io(IoError::Io(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "refused",
        )));
        let e = map_connect_error(refused, "db.example:3306");
        assert!(matches!(&e, Error::Connection { message } if message.contains("db.example:3306")));
        let timed_out =
            mysql_async::Error::Io(IoError::Io(io::Error::new(io::ErrorKind::TimedOut, "slow")));
        assert!(matches!(map_error(timed_out), Error::Timeout { .. }));
        let no_tls = mysql_async::Error::Driver(DriverError::NoClientSslFlagFromServer);
        assert!(is_no_tls_offered(&no_tls));
        assert!(matches!(map_error(no_tls), Error::Connection { .. }));
    }
}
