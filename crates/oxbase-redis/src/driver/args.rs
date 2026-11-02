//! Statement text and bound values → one command's arguments, and what the driver needs to
//! know about a command before it runs (refused, blocking, script, write).

use oxbase_types::{Error, Result, Value};

use crate::command::parse_command_line;

/// The arguments of the one command `sql` (and `params`) stand for.
///
/// - `params` empty: `sql` is one console line ([`parse_command_line`] quoting).
/// - `params` not empty: `sql` holds the command words (read the same way, so `"CONFIG GET"`
///   is two words) and every value is one more argument, in order: [`Value::Bytes`] as is,
///   [`Value::Text`] as UTF-8, booleans as `1` / `0`, JSON as its text, everything else in its
///   display text (numbers in decimal). [`Value::Null`] has no Redis form and is refused.
pub(crate) fn command_args(sql: &str, params: &[Value]) -> Result<Vec<Vec<u8>>> {
    let mut args = parse_command_line(sql)?;
    if args.is_empty() {
        return Err(Error::invalid_input("the Redis command line is empty"));
    }
    for (i, value) in params.iter().enumerate() {
        args.push(
            param_bytes(value).map_err(|e| {
                Error::invalid_input(format!("parameter {}: {}", i + 1, message(&e)))
            })?,
        );
    }
    Ok(args)
}

fn message(e: &Error) -> String {
    match e {
        Error::InvalidInput { message } => message.clone(),
        other => other.to_string(),
    }
}

fn param_bytes(value: &Value) -> Result<Vec<u8>> {
    Ok(match value {
        Value::Null => {
            return Err(Error::invalid_input(
                "NULL has no Redis representation (use an empty string)",
            ));
        }
        Value::Bytes(b) => b.clone(),
        Value::Text(s) | Value::Decimal(s) => s.clone().into_bytes(),
        Value::Bool(b) => if *b { b"1" } else { b"0" }.to_vec(),
        Value::Json(j) => j.to_string().into_bytes(),
        other => other.to_string().into_bytes(),
    })
}

/// How a running command is interrupted on cancel (driver module docs, "Cancel").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelKind {
    /// A blocking command: `CLIENT UNBLOCK <id> ERROR` from a side connection.
    Blocking,
    /// `EVAL` / `EVALSHA` (+ `_RO`): `SCRIPT KILL` from a side connection.
    Script,
    /// `FCALL` / `FCALL_RO`: `FUNCTION KILL` from a side connection.
    Function,
    /// Anything else: the reply is dropped and the connection replaced.
    Replace,
}

/// What the driver reads from a command's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandInfo {
    /// The command name, upper case (`HGETALL`).
    pub name: String,
    /// The second word, upper case, for container commands (`CONFIG GET` → `GET`).
    pub sub: Option<String>,
    pub cancel: CancelKind,
}

/// Transaction commands a cluster session refuses ([`CommandInfo::check_cluster`]).
const CLUSTER_REFUSED: [&str; 5] = ["MULTI", "EXEC", "DISCARD", "WATCH", "UNWATCH"];

/// Commands that switch the connection into a mode the multiplexed session cannot serve, or
/// end it. The console guard blocks them too; the driver refuses them for every caller.
const REFUSED: [(&str, &str); 11] = [
    ("SUBSCRIBE", "use the pub/sub panel to subscribe"),
    ("PSUBSCRIBE", "use the pub/sub panel to subscribe"),
    ("SSUBSCRIBE", "sharded pub/sub is not offered"),
    ("UNSUBSCRIBE", "use the pub/sub panel"),
    ("PUNSUBSCRIBE", "use the pub/sub panel"),
    ("SUNSUBSCRIBE", "sharded pub/sub is not offered"),
    ("MONITOR", "MONITOR takes over the connection"),
    ("SYNC", "replication commands take over the connection"),
    ("PSYNC", "replication commands take over the connection"),
    ("QUIT", "close the connection instead"),
    (
        "RESET",
        "RESET would drop the session's authentication and database",
    ),
];

const BLOCKING: [&str; 10] = [
    "BLPOP",
    "BRPOP",
    "BLMOVE",
    "BRPOPLPUSH",
    "BLMPOP",
    "BZPOPMIN",
    "BZPOPMAX",
    "BZMPOP",
    "WAIT",
    "WAITAOF",
];

/// Commands whose integer reply counts what they changed: rendered as `RowsAffected`.
const WRITES: [&str; 37] = [
    "DEL",
    "UNLINK",
    "HSET",
    "HSETNX",
    "HDEL",
    "LPUSH",
    "RPUSH",
    "LPUSHX",
    "RPUSHX",
    "LREM",
    "SADD",
    "SREM",
    "SMOVE",
    "ZADD",
    "ZREM",
    "ZREMRANGEBYSCORE",
    "ZREMRANGEBYRANK",
    "ZREMRANGEBYLEX",
    "XDEL",
    "XTRIM",
    "XACK",
    "EXPIRE",
    "PEXPIRE",
    "EXPIREAT",
    "PEXPIREAT",
    "PERSIST",
    "RENAMENX",
    "MSETNX",
    "SETNX",
    "COPY",
    "MOVE",
    "SINTERSTORE",
    "SUNIONSTORE",
    "SDIFFSTORE",
    "ZUNIONSTORE",
    "ZINTERSTORE",
    "ZDIFFSTORE",
];

impl CommandInfo {
    /// Reads `args` (not empty).
    pub(crate) fn of(args: &[Vec<u8>]) -> Self {
        let word = |i: usize| {
            args.get(i)
                .map(|a| String::from_utf8_lossy(a).to_ascii_uppercase())
        };
        let name = word(0).unwrap_or_default();
        let sub = word(1);
        let cancel = match name.as_str() {
            n if BLOCKING.contains(&n) => CancelKind::Blocking,
            "XREAD" | "XREADGROUP" if args.iter().any(|a| a.eq_ignore_ascii_case(b"BLOCK")) => {
                CancelKind::Blocking
            }
            "EVAL" | "EVALSHA" | "EVAL_RO" | "EVALSHA_RO" => CancelKind::Script,
            "FCALL" | "FCALL_RO" => CancelKind::Function,
            _ => CancelKind::Replace,
        };
        Self { name, sub, cancel }
    }

    /// `InvalidInput` for the commands the session cannot run (pub/sub, `MONITOR`,
    /// replication, `QUIT`, `RESET`, `HELLO` with a protocol, `CLIENT REPLY`).
    pub(crate) fn check_allowed(&self, args: &[Vec<u8>]) -> Result<()> {
        if let Some((_, why)) = REFUSED.iter().find(|(n, _)| *n == self.name) {
            return Err(Error::invalid_input(format!(
                "{} cannot run in an editor session: {why}",
                self.name
            )));
        }
        if self.name == "HELLO" && args.len() > 1 {
            return Err(Error::invalid_input(
                "HELLO with arguments would switch the session's protocol or credentials; \
                 HELLO alone shows the connection facts",
            ));
        }
        if self.name == "CLIENT" && self.sub.as_deref() == Some("REPLY") {
            return Err(Error::invalid_input(
                "CLIENT REPLY would desynchronise the session's replies",
            ));
        }
        Ok(())
    }

    /// Refuses the transaction commands on a cluster session: the cluster routes `MULTI` /
    /// `EXEC` to any node and each queued command to the node of its key, so a transaction
    /// typed line by line would not be one (writes would apply at once, `DISCARD` would not
    /// undo them).
    pub(crate) fn check_cluster(&self) -> Result<()> {
        if CLUSTER_REFUSED.contains(&self.name.as_str()) {
            return Err(Error::invalid_input(format!(
                "{} is not offered on a cluster session: the cluster sends each command of a \
                 transaction to the node of its key, so it would not be atomic. Open a \
                 standalone profile on the node that holds the keys instead",
                self.name
            )));
        }
        Ok(())
    }

    /// `true` when an integer reply counts changed items ([`WRITES`]).
    pub(crate) fn counts_changes(&self) -> bool {
        WRITES.contains(&self.name.as_str())
    }

    /// `true` for `SELECT`.
    pub(crate) fn is_select(&self) -> bool {
        self.name == "SELECT"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(items: &[&str]) -> Vec<Vec<u8>> {
        items.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    #[test]
    fn console_lines_and_bound_values_become_arguments() {
        assert_eq!(
            command_args(r#"SET "a key" v"#, &[]).unwrap(),
            text(&["SET", "a key", "v"])
        );
        let params = [
            Value::Bytes(vec![0, 0xff, b'\n']),
            Value::Text("grüße".into()),
            Value::Int(-7),
            Value::Float(1.5),
            Value::Bool(true),
            Value::Json(serde_json::json!({"a": 1})),
        ];
        assert_eq!(
            command_args("CONFIG GET", &params).unwrap(),
            vec![
                b"CONFIG".to_vec(),
                b"GET".to_vec(),
                vec![0, 0xff, b'\n'],
                "grüße".as_bytes().to_vec(),
                b"-7".to_vec(),
                b"1.5".to_vec(),
                b"1".to_vec(),
                br#"{"a":1}"#.to_vec(),
            ]
        );
    }

    #[test]
    fn empty_lines_nulls_and_bad_quotes_are_refused() {
        for (sql, params) in [
            ("   ", vec![]),
            ("", vec![Value::Text("x".into())]),
            ("SET k", vec![Value::Null]),
            (r#"SET "k"#, vec![]),
        ] {
            let err = command_args(sql, &params).unwrap_err();
            assert!(matches!(err, Error::InvalidInput { .. }), "{sql:?}: {err}");
        }
        let err = command_args("SET k", &[Value::Int(1), Value::Null]).unwrap_err();
        assert!(err.to_string().contains("parameter 2"), "{err}");
    }

    #[test]
    fn classifies_commands() {
        let info = |line: &str| CommandInfo::of(&command_args(line, &[]).unwrap());
        assert_eq!(info("blpop q 0").cancel, CancelKind::Blocking);
        assert_eq!(
            info("XREAD COUNT 1 block 0 STREAMS s $").cancel,
            CancelKind::Blocking
        );
        assert_eq!(
            info("XREAD COUNT 1 STREAMS s 0").cancel,
            CancelKind::Replace
        );
        assert_eq!(info("EVAL 'return 1' 0").cancel, CancelKind::Script);
        assert_eq!(info("fcall_ro f 0").cancel, CancelKind::Function);
        assert_eq!(info("GET k").cancel, CancelKind::Replace);
        let config = info("config get maxmemory");
        assert_eq!(config.name, "CONFIG");
        assert_eq!(config.sub.as_deref(), Some("GET"));
        assert!(info("del a b").counts_changes());
        assert!(!info("INCR a").counts_changes());
        assert!(info("select 3").is_select());
    }

    #[test]
    fn refuses_commands_that_take_over_the_session() {
        for line in [
            "SUBSCRIBE c",
            "psubscribe m7:*",
            "MONITOR",
            "QUIT",
            "RESET",
            "HELLO 3",
            "CLIENT REPLY OFF",
            "SYNC",
        ] {
            let args = command_args(line, &[]).unwrap();
            let err = CommandInfo::of(&args).check_allowed(&args).unwrap_err();
            assert!(matches!(err, Error::InvalidInput { .. }), "{line}: {err}");
        }
        for line in ["HELLO", "CLIENT ID", "PUBLISH c m", "GET k"] {
            let args = command_args(line, &[]).unwrap();
            CommandInfo::of(&args).check_allowed(&args).unwrap();
        }
    }

    #[test]
    fn cluster_sessions_refuse_console_transactions() {
        for line in ["MULTI", "exec", "DISCARD", "WATCH k", "unwatch"] {
            let args = command_args(line, &[]).unwrap();
            let info = CommandInfo::of(&args);
            info.check_allowed(&args).unwrap();
            let err = info.check_cluster().unwrap_err();
            assert!(matches!(err, Error::InvalidInput { .. }), "{line}: {err}");
            assert!(err.to_string().contains("cluster"), "{err}");
        }
        for line in ["GET k", "SET k v", "SCAN 0", "CLUSTER INFO"] {
            let args = command_args(line, &[]).unwrap();
            CommandInfo::of(&args).check_cluster().unwrap();
        }
    }
}
