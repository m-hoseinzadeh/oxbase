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
