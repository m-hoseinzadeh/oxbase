//! rusqlite errors -> oxbase errors.

use oxbase_types::Error;
use rusqlite::ffi::ErrorCode;

/// Maps a rusqlite error.
///
/// SQLite failures become [`Error::Sql`] with the primary result-code name (`SQLITE_READONLY`,
/// `SQLITE_CONSTRAINT`, ...) as the code; `SQLITE_INTERRUPT` becomes [`Error::Cancelled`]
/// (only this driver interrupts statements, and only to cancel them).
pub(crate) fn map_err(e: rusqlite::Error) -> Error {
    use rusqlite::Error as E;
    match e {
        E::SqliteFailure(f, msg) => {
            if f.code == ErrorCode::OperationInterrupted {
                return Error::Cancelled;
            }
            let message = msg.unwrap_or_else(|| f.to_string());
            Error::sql(Some(code_name(f.extended_code)), message)
        }
        E::SqlInputError {
            error, msg, offset, ..
        } => {
            if error.code == ErrorCode::OperationInterrupted {
                return Error::Cancelled;
            }
            let message = if offset >= 0 {
                format!("{msg} (at byte {offset})")
            } else {
                msg
            };
            Error::sql(Some(code_name(error.extended_code)), message)
        }
        E::InvalidParameterCount(..)
        | E::InvalidParameterName(_)
        | E::NulError(_)
        | E::InvalidPath(_)
        | E::MultipleStatement
        | E::ToSqlConversionFailure(_) => Error::invalid_input(e.to_string()),
        other => Error::internal(format!("sqlite: {other}")),
    }
}

/// The SQLite name of a (primary or extended) result code's primary part.
pub(crate) fn code_name(code: i32) -> &'static str {
    match code & 0xff {
        0 => "SQLITE_OK",
        1 => "SQLITE_ERROR",
        2 => "SQLITE_INTERNAL",
        3 => "SQLITE_PERM",
        4 => "SQLITE_ABORT",
        5 => "SQLITE_BUSY",
        6 => "SQLITE_LOCKED",
        7 => "SQLITE_NOMEM",
        8 => "SQLITE_READONLY",
        9 => "SQLITE_INTERRUPT",
        10 => "SQLITE_IOERR",
        11 => "SQLITE_CORRUPT",
        12 => "SQLITE_NOTFOUND",
        13 => "SQLITE_FULL",
        14 => "SQLITE_CANTOPEN",
        15 => "SQLITE_PROTOCOL",
        16 => "SQLITE_EMPTY",
        17 => "SQLITE_SCHEMA",
        18 => "SQLITE_TOOBIG",
        19 => "SQLITE_CONSTRAINT",
        20 => "SQLITE_MISMATCH",
        21 => "SQLITE_MISUSE",
        22 => "SQLITE_NOLFS",
        23 => "SQLITE_AUTH",
        24 => "SQLITE_FORMAT",
        25 => "SQLITE_RANGE",
        26 => "SQLITE_NOTADB",
        27 => "SQLITE_NOTICE",
        28 => "SQLITE_WARNING",
        _ => "SQLITE_UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::ffi;

    #[test]
    fn maps_codes() {
        let e = map_err(rusqlite::Error::SqliteFailure(
            ffi::Error::new(ffi::SQLITE_READONLY),
            Some("attempt to write a readonly database".into()),
        ));
        assert_eq!(e.sql_code(), Some("SQLITE_READONLY"));
        assert!(e.to_string().contains("readonly"), "{e}");
        // Extended code (SQLITE_CONSTRAINT_UNIQUE = 2067) keeps its primary name.
        let e = map_err(rusqlite::Error::SqliteFailure(ffi::Error::new(2067), None));
        assert_eq!(e.sql_code(), Some("SQLITE_CONSTRAINT"));
        let e = map_err(rusqlite::Error::SqliteFailure(
            ffi::Error::new(ffi::SQLITE_INTERRUPT),
            None,
        ));
        assert!(e.is_cancelled());
        let e = map_err(rusqlite::Error::InvalidParameterCount(1, 2));
        assert_eq!(e.category(), oxbase_types::ErrorCategory::InvalidInput);
    }
}
