//! `redis` crate errors → [`oxbase_types::Error`].
//!
//! Server error replies keep their first word as the code (`WRONGTYPE`, `NOPERM`, `ERR`, ...);
//! `WRONGPASS` / `NOAUTH` and failed `AUTH` / `HELLO ... AUTH` are [`Error::Auth`]; socket and
//! protocol failures are [`Error::Connection`]. The `redis` crate never puts passwords into its
//! errors, and neither does this module.

use oxbase_types::Error;
use redis::{ErrorKind, RedisError, ServerError};

/// Codes of server errors that mean the credentials were refused.
const AUTH_CODES: [&str; 2] = ["WRONGPASS", "NOAUTH"];

/// Maps a client-side or server error of the `redis` crate.
pub(crate) fn from_redis(e: &RedisError) -> Error {
    let code = e.code().map(str::to_owned);
    if e.kind() == ErrorKind::AuthenticationFailed
        || code.as_deref().is_some_and(|c| AUTH_CODES.contains(&c))
    {
        return Error::auth(format!("Redis refused the credentials: {}", detail(e)));
    }
    if e.is_timeout() {
        return Error::timeout(format!("Redis did not answer in time: {e}"));
    }
    match e.kind() {
        ErrorKind::Server(_) | ErrorKind::Extension => Error::Sql {
            code,
            message: detail(e),
        },
        ErrorKind::InvalidClientConfig | ErrorKind::EmptySentinelList => {
            Error::invalid_input(format!("Redis client configuration: {e}"))
        }
        ErrorKind::RESP3NotSupported => {
            Error::unsupported("the server does not speak RESP3 (Redis 6.0 or newer is needed)")
        }
        ErrorKind::MasterNameNotFoundBySentinel | ErrorKind::NoValidReplicasFoundBySentinel => {
            Error::not_found(format!("Redis sentinel: {e}"))
        }
        ErrorKind::Parse | ErrorKind::UnexpectedReturnType => {
            Error::connection(format!("Redis protocol error: {e}"))
        }
        _ => Error::connection(format!("Redis connection: {e}")),
    }
}

/// An error reply (`-WRONGTYPE Operation against a key ...`) as [`Error::Sql`] with its first
/// word as the code.
pub(crate) fn from_server(e: &ServerError) -> Error {
    let message = match e.details() {
        Some(d) => d.to_owned(),
        None => e.code().to_owned(),
    };
    if AUTH_CODES.contains(&e.code()) {
        return Error::auth(format!("Redis refused the credentials: {message}"));
    }
    Error::Sql {
        code: Some(e.code().to_owned()),
        message,
    }
}

/// `true` when `e` says the server cannot speak RESP3 (`HELLO` unknown: Redis before 6.0).
pub(crate) fn is_resp3_refused(e: &RedisError) -> bool {
    if e.kind() == ErrorKind::RESP3NotSupported {
        return true;
    }
    let text = e.to_string().to_ascii_lowercase();
    e.code() == Some("NOPROTO") || (text.contains("unknown command") && text.contains("hello"))
}

/// `true` when the connection behind `e` is gone (the link must be replaced).
pub(crate) fn is_link_broken(e: &RedisError) -> bool {
    e.is_io_error()
        || e.is_connection_dropped()
        || e.is_connection_refusal()
        || matches!(
            e.kind(),
            ErrorKind::Io | ErrorKind::Parse | ErrorKind::ClusterConnectionNotFound
        )
}

/// The server's message without the code word.
fn detail(e: &RedisError) -> String {
    e.detail().map_or_else(|| e.to_string(), str::to_owned)
}
