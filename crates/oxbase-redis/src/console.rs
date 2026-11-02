//! The Redis console language (shell stream): how the query editor splits, guards and
//! redacts console scripts. Registered as the Redis `ScriptLanguage` in
//! [`crate::dialect::register`]. Contract: `docs/m7-contracts.md` §4.3.
//!
//! # Statements
//!
//! - One command per line ([`crate::command::parse_command_line`] reads its arguments). A line
//!   is a statement unless it is blank or a comment (first non-blank characters `#` or `//`).
//!   A quoted argument cannot span lines (a line with an unterminated quote is still one
//!   statement; running it fails with `InvalidInput`).
//! - The statement's range is the line without leading / trailing whitespace; `segment_end` is
//!   the end of the line including its `\n`, so "run current statement" on a blank line after a
//!   command picks the next command (as the SQL splitter does).
//! - Kind: `Query` for commands that return data (`GET`, `HGETALL`, `SCAN`, `INFO`, ... any
//!   command whose first word is in the read-only command list of the Redis docs), else
//!   `Other`. A hint only.
//!
//! # Guards (`ScriptLanguage::guard`)
//!
//! | Command | Guard |
//! |---|---|
//! | `FLUSHALL`, `FLUSHDB`, `SWAPDB`, `SHUTDOWN`, `DEBUG`, `SCRIPT FLUSH`, `FUNCTION FLUSH`, `FUNCTION DELETE`, `CLUSTER RESET`, `CLUSTER FAILOVER`, `CLUSTER FORGET`, `REPLICAOF` / `SLAVEOF`, `CONFIG SET`, `CONFIG REWRITE`, `ACL DELUSER`, `ACL SETUSER`, `KEYS` (blocks the server on a big database: use the key browser) | `Confirm` with a sentence saying what it does |
//! | `SUBSCRIBE`, `PSUBSCRIBE`, `SSUBSCRIBE`, `MONITOR`, `SYNC`, `PSYNC`, `QUIT`, `RESET`, `HELLO` with a protocol switch, `CLIENT REPLY` | `Blocked`: pub/sub has its own panel; the others break the session's request / reply flow |
//!
//! Matching is case-insensitive on the first one or two words.
//!
//! # Redaction (`ScriptLanguage::redact_secrets`)
//!
//! Replaced by `***` (the rest of the line kept): the password of `AUTH [user] password`, of
//! `HELLO ... AUTH user password`, `MIGRATE ... AUTH password` / `AUTH2 user password`; the
//! `>password`, `<password`, `#hash` and `!hash` rules of `ACL SETUSER`; the value of
//! `CONFIG SET requirepass|masterauth|masteruser|tls-key-file-pass|tls-client-key-file-pass`.
//! Every line of a multi-line text is redacted (history keeps one statement per entry, error
//! messages may quote several lines).

use std::borrow::Cow;

use oxbase_dialect::{ScriptLanguage, StatementGuard};
use oxbase_sql::Statement;

/// The Redis console as an editor language. See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct RedisConsole;

impl ScriptLanguage for RedisConsole {
    fn name(&self) -> &'static str {
        "redis-console"
    }

    fn editor_language(&self) -> &'static str {
        "redis"
    }

    fn split(&self, text: &str) -> Vec<Statement> {
        let _ = text;
        unimplemented!("m7: shell")
    }

    fn redact_secrets<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let _ = text;
        unimplemented!("m7: shell")
    }

    fn guard(&self, statement: &Statement) -> StatementGuard {
        let _ = statement;
        unimplemented!("m7: shell")
    }
}
