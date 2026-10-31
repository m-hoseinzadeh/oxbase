//! `tokio-postgres` errors and notices -> oxbase types.
//!
//! Messages come from the server or from `tokio-postgres` / rustls / the OS and never contain
//! the password: the driver hands it to `tokio-postgres` only as connection configuration, which
//! is not part of any error.

use std::error::Error as StdError;
use std::io;

use oxbase_driver::{Notice, NoticeSeverity};
use oxbase_types::Error;
use tokio_postgres::error::{DbError, Severity, SqlState};

/// The server message with its DETAIL and HINT lines.
fn db_message(db: &DbError) -> String {
    let mut msg = db.message().to_owned();
    if let Some(detail) = db.detail() {
        msg.push_str("\nDETAIL: ");
        msg.push_str(detail);
    }
    if let Some(hint) = db.hint() {
        msg.push_str("\nHINT: ");
        msg.push_str(hint);
    }
    msg
}

/// A server error report as an oxbase error.
///
/// `query_canceled` (57014) stays an SQL error with the server's message: it also reports
/// `statement_timeout` and `pg_cancel_backend()` from another session, which are not a cancel
/// the user asked for here. A cancel this driver sent is reported as [`Error::Cancelled`] by the
/// statement job itself (see `conn.rs`), which never forwards the server's error in that case.
pub(crate) fn from_db_error(db: &DbError) -> Error {
    let code = db.code().code();
    let message = db_message(db);
    match &code[..2] {
        // invalid_authorization_specification, invalid_password, no pg_hba.conf entry
        "28" => Error::auth(message),
        // connection_exception
        "08" => Error::connection(message),
        _ if *db.code() == SqlState::INVALID_CATALOG_NAME => Error::not_found(message),
        // too_many_connections, admin/crash shutdown, cannot_connect_now
        _ if *db.code() == SqlState::TOO_MANY_CONNECTIONS || code.starts_with("57P") => {
            Error::connection(message)
        }
        _ => Error::sql(Some(code), message),
    }
}

/// `err` with its source chain (`tokio-postgres` keeps the cause out of `Display`).
fn with_sources(err: &dyn StdError) -> String {
    let mut msg = err.to_string();
    let mut source = err.source();
    while let Some(s) = source {
        let text = s.to_string();
        if !text.is_empty() && !msg.ends_with(&text) {
            msg.push_str(": ");
            msg.push_str(&text);
        }
        source = s.source();
    }
    msg
}

fn io_kind(err: &dyn StdError) -> Option<io::ErrorKind> {
    let mut source: Option<&(dyn StdError + 'static)> = err.source();
    while let Some(s) = source {
        if let Some(io) = s.downcast_ref::<io::Error>() {
            return Some(io.kind());
        }
        source = s.source();
    }
    None
}

/// Any `tokio-postgres` error as an oxbase error.
pub(crate) fn from_pg(err: &tokio_postgres::Error) -> Error {
    if let Some(db) = err.as_db_error() {
        return from_db_error(db);
    }
    if err.is_closed() {
        return Error::connection("the connection to the server is closed");
    }
    let msg = with_sources(err);
    if io_kind(err) == Some(io::ErrorKind::TimedOut) || msg.starts_with("timeout") {
        return Error::timeout(msg);
    }
    if msg.starts_with("authentication error") || msg.contains("password missing") {
        return Error::auth(msg);
    }
    if msg.starts_with("invalid configuration") || msg.starts_with("invalid connection string") {
        return Error::invalid_input(msg);
    }
    if msg.starts_with("error serializing parameter") {
        return Error::invalid_input(msg);
    }
    if msg.starts_with("error performing TLS handshake") {
        return Error::connection(format!("TLS: {msg}"));
    }
    if msg.starts_with("error connecting")
        || msg.starts_with("error communicating")
        || msg.starts_with("unexpected message")
        || msg.starts_with("error parsing response")
    {
        return Error::connection(msg);
    }
    Error::internal(msg)
}

/// A NOTICE / WARNING / INFO message as a [`Notice`].
pub(crate) fn notice_from_db(db: &DbError) -> Notice {
    let severity = match db.parsed_severity() {
        Some(Severity::Warning) => NoticeSeverity::Warning,
        Some(Severity::Debug) => NoticeSeverity::Debug,
        Some(_) => NoticeSeverity::Info,
        None => match db.severity() {
            "WARNING" => NoticeSeverity::Warning,
            "DEBUG" => NoticeSeverity::Debug,
            _ => NoticeSeverity::Info,
        },
    };
    Notice {
        severity,
        message: db_message(db),
        code: Some(db.code().code().to_owned()),
    }
}
