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
//! | `SUBSCRIBE`, `PSUBSCRIBE`, `SSUBSCRIBE` (and their `UNSUBSCRIBE` forms), `MONITOR`, `SYNC`, `PSYNC`, `QUIT`, `RESET`, `HELLO` with a protocol switch, `CLIENT REPLY` | `Blocked`: pub/sub has its own panel; the others break the session's request / reply flow |
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
use std::ops::Range;

use oxbase_dialect::script::{statement, trim_range};
use oxbase_dialect::{ScriptLanguage, StatementGuard};
use oxbase_sql::{Statement, StatementKind};

/// What a redacted secret is replaced by.
const REDACTED: &str = "***";

/// `CONFIG SET` parameters whose values are secrets (or name the account a secret belongs to).
const SECRET_CONFIG_PARAMETERS: [&str; 5] = [
    "requirepass",
    "masterauth",
    "masteruser",
    "tls-key-file-pass",
    "tls-client-key-file-pass",
];

/// Commands that only read (the `readonly` flag of the Redis command docs, plus the read
/// subcommands of the container commands below). `StatementKind::Query` for them.
const READ_COMMANDS: &[&str] = &[
    "BITCOUNT",
    "BITFIELD_RO",
    "BITPOS",
    "DBSIZE",
    "DUMP",
    "ECHO",
    "EVALSHA_RO",
    "EVAL_RO",
    "EXISTS",
    "EXPIRETIME",
    "FCALL_RO",
    "GEODIST",
    "GEOHASH",
    "GEOPOS",
    "GEORADIUSBYMEMBER_RO",
    "GEORADIUS_RO",
    "GEOSEARCH",
    "GET",
    "GETBIT",
    "GETRANGE",
    "HEXISTS",
    "HGET",
    "HGETALL",
    "HKEYS",
    "HLEN",
    "HMGET",
    "HRANDFIELD",
    "HSCAN",
    "HSTRLEN",
    "HVALS",
    "INFO",
    "JSON.ARRLEN",
    "JSON.GET",
    "JSON.MGET",
    "JSON.OBJKEYS",
    "JSON.OBJLEN",
    "JSON.STRLEN",
    "JSON.TYPE",
    "KEYS",
    "LASTSAVE",
    "LCS",
    "LINDEX",
    "LLEN",
    "LOLWUT",
    "LPOS",
    "LRANGE",
    "MGET",
    "PEXPIRETIME",
    "PFCOUNT",
    "PING",
    "PTTL",
    "RANDOMKEY",
    "ROLE",
    "SCAN",
    "SCARD",
    "SDIFF",
    "SINTER",
    "SINTERCARD",
    "SISMEMBER",
    "SMEMBERS",
    "SMISMEMBER",
    "SORT_RO",
    "SRANDMEMBER",
    "SSCAN",
    "STRLEN",
    "SUBSTR",
    "SUNION",
    "TIME",
    "TTL",
    "TYPE",
    "XLEN",
    "XPENDING",
    "XRANGE",
    "XREAD",
    "XREVRANGE",
    "ZCARD",
    "ZCOUNT",
    "ZDIFF",
    "ZINTER",
    "ZINTERCARD",
    "ZLEXCOUNT",
    "ZMSCORE",
    "ZRANDMEMBER",
    "ZRANGE",
    "ZRANGEBYLEX",
    "ZRANGEBYSCORE",
    "ZRANK",
    "ZREVRANGE",
    "ZREVRANGEBYLEX",
    "ZREVRANGEBYSCORE",
    "ZREVRANK",
    "ZSCAN",
    "ZSCORE",
    "ZUNION",
];

/// Container commands whose subcommands below only read.
const READ_SUBCOMMANDS: &[(&str, &[&str])] = &[
    ("ACL", &["CAT", "GETUSER", "LIST", "LOG", "USERS", "WHOAMI"]),
    ("CLIENT", &["GETNAME", "ID", "INFO", "LIST"]),
    (
        "CLUSTER",
        &[
            "COUNTKEYSINSLOT",
            "GETKEYSINSLOT",
            "INFO",
            "KEYSLOT",
            "MYID",
            "NODES",
            "SHARDS",
            "SLOTS",
        ],
    ),
    ("COMMAND", &["COUNT", "DOCS", "GETKEYS", "INFO", "LIST"]),
    ("CONFIG", &["GET"]),
    ("FUNCTION", &["DUMP", "LIST", "STATS"]),
    ("LATENCY", &["DOCTOR", "HISTOGRAM", "HISTORY", "LATEST"]),
    ("MEMORY", &["DOCTOR", "STATS", "USAGE"]),
    ("MODULE", &["LIST"]),
    ("OBJECT", &["ENCODING", "FREQ", "IDLETIME", "REFCOUNT"]),
    (
        "PUBSUB",
        &[
            "CHANNELS",
            "NUMPAT",
            "NUMSUB",
            "SHARDCHANNELS",
            "SHARDNUMSUB",
        ],
    ),
    ("SCRIPT", &["EXISTS"]),
    ("SLOWLOG", &["GET", "LEN"]),
    ("XINFO", &["CONSUMERS", "GROUPS", "STREAM"]),
];

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
        let mut out = Vec::new();
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            let range = offset..offset + line.len();
            offset = range.end;
            let Some(trimmed) = trim_range(text, range.clone()) else {
                continue;
            };
            let body = &text[trimmed.clone()];
            if is_comment(body) {
                continue;
            }
            let kind = statement_kind(&lex(body));
            out.push(statement(text, trimmed, range.end, kind));
        }
        out
    }

    fn redact_secrets<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out: Option<String> = None;
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            let redacted = redact_line(line);
            if out.is_none() && redacted.is_some() {
                let mut s = String::with_capacity(text.len() + 8);
                s.push_str(&text[..offset]);
                out = Some(s);
            }
            if let Some(out) = out.as_mut() {
                out.push_str(redacted.as_deref().unwrap_or(line));
            }
            offset += line.len();
        }
        out.map_or(Cow::Borrowed(text), Cow::Owned)
    }

    fn guard(&self, statement: &Statement) -> StatementGuard {
        let args = lex(&statement.text);
        let words: Vec<String> = args.iter().take(2).map(|a| upper(&a.value)).collect();
        guard_for(&words, &args)
    }
}

/// Whether a trimmed line is a comment.
fn is_comment(line: &str) -> bool {
    line.starts_with('#') || line.starts_with("//")
}

/// One argument of a command line: its bytes in the line (quotes included) and its value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Arg {
    span: Range<usize>,
    value: Vec<u8>,
}

fn is_separator(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// The arguments of `line` with the quoting rules of [`crate::command::parse_command_line`],
/// but lenient: an unterminated quote runs to the end of the line and a closing quote may be
/// followed by more of the same argument. Used for guards and redaction, which must also work
/// on lines the server will refuse.
fn lex(line: &str) -> Vec<Arg> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        while i < b.len() && is_separator(b[i]) {
            i += 1;
        }
        if i >= b.len() {
            return out;
        }
        let start = i;
        let mut value = Vec::new();
        while i < b.len() && !is_separator(b[i]) {
            match b[i] {
                b'"' => i = lex_double(b, i + 1, &mut value),
                b'\'' => i = lex_single(b, i + 1, &mut value),
                c => {
                    value.push(c);
                    i += 1;
                }
            }
        }
        out.push(Arg {
            span: start..i,
            value,
        });
    }
}

/// A `"..."` body from `i` (after the quote); returns the index after the closing quote, or
/// the end of the line.
fn lex_double(b: &[u8], mut i: usize, value: &mut Vec<u8>) -> usize {
    while i < b.len() {
        match b[i] {
            b'"' => return i + 1,
            b'\\' if i + 1 < b.len() => {
                let next = b[i + 1];
                if next == b'x'
                    && i + 3 < b.len()
                    && let (Some(hi), Some(lo)) = (hex_digit(b[i + 2]), hex_digit(b[i + 3]))
                {
                    value.push((hi << 4) | lo);
                    i += 4;
                    continue;
                }
                value.push(match next {
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'b' => 0x08,
                    b'a' => 0x07,
                    other => other,
                });
                i += 2;
            }
            c => {
                value.push(c);
                i += 1;
            }
        }
    }
    i
}

/// A `'...'` body from `i` (after the quote); only `\'` is an escape.
fn lex_single(b: &[u8], mut i: usize, value: &mut Vec<u8>) -> usize {
    while i < b.len() {
        match b[i] {
            b'\'' => return i + 1,
            b'\\' if b.get(i + 1) == Some(&b'\'') => {
                value.push(b'\'');
                i += 2;
            }
            c => {
                value.push(c);
                i += 1;
            }
        }
    }
    i
}

/// An argument as upper-case text (lossy for non-UTF-8 bytes; command names are ASCII).
fn upper(value: &[u8]) -> String {
    String::from_utf8_lossy(value).to_ascii_uppercase()
}

fn statement_kind(args: &[Arg]) -> StatementKind {
    let Some(first) = args.first() else {
        return StatementKind::Other;
    };
    let command = upper(&first.value);
    if READ_COMMANDS.contains(&command.as_str()) {
        return StatementKind::Query;
    }
    let sub = args.get(1).map(|a| upper(&a.value)).unwrap_or_default();
    let read = READ_SUBCOMMANDS
        .iter()
        .any(|(c, subs)| *c == command && subs.contains(&sub.as_str()));
    if read {
        StatementKind::Query
    } else {
        StatementKind::Other
    }
}

/// The guard of a command whose first two words (upper case) are `words`.
fn guard_for(words: &[String], args: &[Arg]) -> StatementGuard {
    let Some(command) = words.first() else {
        return StatementGuard::Allowed;
    };
    let sub = words.get(1).map_or("", String::as_str);
    let confirm = |why: &str| StatementGuard::Confirm(why.to_owned());
    let block = |why: &str| StatementGuard::Blocked(why.to_owned());
    match (command.as_str(), sub) {
        (
            "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" | "UNSUBSCRIBE" | "PUNSUBSCRIBE"
            | "SUNSUBSCRIBE",
            _,
        ) => block(&format!(
            "{command} turns the editor's session into a subscription; use the publish / \
             subscribe panel to receive messages"
        )),
        ("MONITOR", _) => block(
            "MONITOR streams every command the server runs and never returns; it cannot run \
             in the console",
        ),
        ("SYNC" | "PSYNC", _) => block(&format!(
            "{command} starts a replication stream; it cannot run in the console"
        )),
        ("QUIT", _) => block("QUIT closes the editor's connection; close the editor instead"),
        ("RESET", _) => block(
            "RESET drops the session's database, name and authentication; open a new editor \
             instead",
        ),
        ("HELLO", version)
            if !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit()) =>
        {
            block(
                "HELLO with a protocol version switches the reply protocol of the editor's \
                 session; choose the protocol in the connection profile",
            )
        }
        ("CLIENT", "REPLY") => {
            block("CLIENT REPLY turns the server's replies off, so the console would wait forever")
        }
        ("FLUSHALL", _) => confirm("FLUSHALL deletes every key of every database on the server"),
        ("FLUSHDB", _) => confirm("FLUSHDB deletes every key of the current database"),
        ("SWAPDB", _) => confirm("SWAPDB swaps two databases for every client of the server"),
        ("SHUTDOWN", _) => confirm("SHUTDOWN stops the Redis server"),
        ("DEBUG", _) => confirm("DEBUG commands can block, crash or corrupt the server"),
        ("SCRIPT", "FLUSH") => confirm("SCRIPT FLUSH removes every cached Lua script"),
        ("FUNCTION", "FLUSH") => confirm("FUNCTION FLUSH deletes every function library"),
        ("FUNCTION", "DELETE") => confirm("FUNCTION DELETE deletes a function library"),
        ("CLUSTER", "RESET" | "FAILOVER" | "FORGET") => {
            confirm(&format!("CLUSTER {sub} changes the cluster's topology"))
        }
        ("REPLICAOF" | "SLAVEOF", _) => confirm(&format!(
            "{command} changes the server's replication (a new replica discards its data)"
        )),
        ("CONFIG", "SET") => confirm("CONFIG SET changes the running server's configuration"),
        ("CONFIG", "REWRITE") => {
            confirm("CONFIG REWRITE overwrites the server's configuration file")
        }
        ("ACL", "DELUSER") => confirm("ACL DELUSER deletes users and disconnects their clients"),
        ("ACL", "SETUSER") => {
            confirm("ACL SETUSER creates or changes a user's passwords and permissions")
        }
        ("KEYS", _) => {
            let pattern = args
                .get(1)
                .map(|a| String::from_utf8_lossy(&a.value).into_owned())
                .unwrap_or_default();
            confirm(&format!(
                "KEYS {pattern} reads the whole keyspace in one blocking call and stalls the \
                 server on a big database (the key browser uses SCAN)"
            ))
        }
        _ => StatementGuard::Allowed,
    }
}

/// `line` with its secrets replaced, or `None` when it has none.
fn redact_line(line: &str) -> Option<String> {
    let args = lex(line);
    let secrets = secret_args(&args);
    if secrets.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(line.len());
    let mut at = 0;
    for (index, keep_prefix) in secrets {
        let arg = &args[index];
        out.push_str(&line[at..arg.span.start]);
        if keep_prefix {
            // `>password` → `>***`: the rule's kind stays readable.
            if let Some(&prefix) = arg.value.first() {
                out.push(char::from(prefix));
            }
        }
        out.push_str(REDACTED);
        at = arg.span.end;
    }
    out.push_str(&line[at..]);
    Some(out)
}

/// The indexes of the arguments of `args` that are secrets, in order, and whether the
/// redaction keeps the first character (`ACL SETUSER` rules).
fn secret_args(args: &[Arg]) -> Vec<(usize, bool)> {
    let word = |i: usize| args.get(i).map(|a| upper(&a.value)).unwrap_or_default();
    let command = word(0);
    let mut out = Vec::new();
    match command.as_str() {
        // AUTH password | AUTH user password: the last argument (every argument after the
        // command word when the line has too many, so nothing slips through).
        "AUTH" => match args.len() {
            0 | 1 => {}
            2 | 3 => out.push((args.len() - 1, false)),
            n => out.extend((1..n).map(|i| (i, false))),
        },
        // HELLO [protover [AUTH username password] [SETNAME clientname]]
        "HELLO" => {
            for i in 1..args.len() {
                if word(i) == "AUTH" && i + 2 < args.len() {
                    out.push((i + 2, false));
                }
            }
        }
        // MIGRATE host port key|"" db timeout [COPY] [REPLACE] [AUTH password]
        // [AUTH2 username password] [KEYS key ...]
        "MIGRATE" => {
            let mut i = 1;
            while i < args.len() {
                match word(i).as_str() {
                    "KEYS" => break,
                    "AUTH" if i + 1 < args.len() => {
                        out.push((i + 1, false));
                        i += 1;
                    }
                    "AUTH2" if i + 2 < args.len() => {
                        out.push((i + 2, false));
                        i += 2;
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        // ACL SETUSER name rule...: `>password`, `<password`, `#hash`, `!hash`.
        "ACL" if word(1) == "SETUSER" => {
            for (i, arg) in args.iter().enumerate().skip(3) {
                if matches!(arg.value.first(), Some(b'>' | b'<' | b'#' | b'!')) {
                    out.push((i, true));
                }
            }
        }
        // CONFIG SET parameter value [parameter value ...]
        "CONFIG" if word(1) == "SET" => {
            let mut i = 2;
            while i + 1 < args.len() {
                let parameter = String::from_utf8_lossy(&args[i].value).to_ascii_lowercase();
                if SECRET_CONFIG_PARAMETERS.contains(&parameter.as_str()) {
                    out.push((i + 1, false));
                }
                i += 2;
            }
        }
        // SENTINEL SET master option value [option value ...]
        "SENTINEL" if word(1) == "SET" => {
            let mut i = 3;
            while i + 1 < args.len() {
                let option = String::from_utf8_lossy(&args[i].value).to_ascii_lowercase();
                if option == "auth-pass" {
                    out.push((i + 1, false));
                }
                i += 2;
            }
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn texts(script: &str) -> Vec<String> {
        RedisConsole
            .split(script)
            .into_iter()
            .map(|s| s.text)
            .collect()
    }

    fn guard(line: &str) -> StatementGuard {
        let s = RedisConsole.split(line);
        assert_eq!(s.len(), 1, "{line}");
        RedisConsole.guard(&s[0])
    }

    #[test]
    fn one_statement_per_line_without_blanks_and_comments() {
        let cases: &[(&str, &[&str])] = &[
            ("", &[]),
            ("   \n\n\t\n", &[]),
            ("PING", &["PING"]),
            ("  GET a  \r\nSET b 1\n", &["GET a", "SET b 1"]),
            ("# note\n// other\n  #indented\nDBSIZE", &["DBSIZE"]),
            ("SET k \"a # b\"\n", &["SET k \"a # b\""]),
            ("SET k \"open\nGET k", &["SET k \"open", "GET k"]),
            ("MULTI\nINCR n\nEXEC\n", &["MULTI", "INCR n", "EXEC"]),
            ("GET ü\nGET \"\\xff\"", &["GET ü", "GET \"\\xff\""]),
        ];
        for (script, want) in cases {
            assert_eq!(texts(script), *want, "{script:?}");
        }
    }

    #[test]
    fn statement_ranges_and_positions() {
        let script = "  GET a\n\n# c\nSET ü 1\n";
        let s = RedisConsole.split(script);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].range, 2..7);
        assert_eq!(s[0].segment_end, 8);
        assert_eq!((s[0].start.line, s[0].start.column), (1, 3));
        assert_eq!(s[1].text, "SET ü 1");
        assert_eq!(&script[s[1].range.clone()], "SET ü 1");
        assert_eq!(s[1].segment_end, script.len());
        assert_eq!((s[1].start.line, s[1].end.column), (4, 8));
    }

    #[test]
    fn kinds_follow_the_read_only_commands() {
        let cases = [
            ("GET a", StatementKind::Query),
            ("hgetall h", StatementKind::Query),
            ("ZRANGE z 0 -1 WITHSCORES", StatementKind::Query),
            ("CONFIG GET maxmemory", StatementKind::Query),
            ("config set maxmemory 1", StatementKind::Other),
            ("SET a 1", StatementKind::Other),
            ("CLIENT LIST", StatementKind::Query),
            ("\"GET\" a", StatementKind::Query),
        ];
        for (line, kind) in cases {
            assert_eq!(RedisConsole.split(line)[0].kind, kind, "{line}");
        }
    }

    #[test]
    fn guards_confirm_and_block_the_listed_commands() {
        let confirm = [
            "FLUSHALL",
            "flushall async",
            "FLUSHDB",
            "SWAPDB 0 1",
            "SHUTDOWN NOSAVE",
            "DEBUG SLEEP 1",
            "SCRIPT FLUSH",
            "FUNCTION FLUSH",
            "function delete lib",
            "CLUSTER RESET HARD",
            "CLUSTER FAILOVER",
            "CLUSTER FORGET abc",
            "REPLICAOF host 6379",
            "SLAVEOF NO ONE",
            "CONFIG SET maxmemory 10mb",
            "CONFIG REWRITE",
            "ACL DELUSER bob",
            "ACL SETUSER bob on >pw",
            "KEYS *",
            "\"FLUSHDB\"",
        ];
        for line in confirm {
            match guard(line) {
                StatementGuard::Confirm(why) => assert!(!why.is_empty(), "{line}"),
                other => panic!("{line}: {other:?}"),
            }
        }
        let blocked = [
            "SUBSCRIBE news",
            "psubscribe m7:*",
            "SSUBSCRIBE s",
            "UNSUBSCRIBE",
            "MONITOR",
            "SYNC",
            "PSYNC ? -1",
            "QUIT",
            "RESET",
            "HELLO 3",
            "HELLO 2 AUTH u p",
            "CLIENT REPLY OFF",
        ];
        for line in blocked {
            assert!(
                matches!(guard(line), StatementGuard::Blocked(_)),
                "{line}: {:?}",
                guard(line)
            );
        }
        let allowed = [
            "GET flushdb",
            "SET KEYS 1",
            "HELLO",
            "CLIENT LIST",
            "SCRIPT EXISTS abc",
            "CONFIG GET *",
            "ACL LIST",
            "CLUSTER INFO",
            "FUNCTION LIST",
            "PUBLISH ch msg",
            "MULTI",
        ];
        for line in allowed {
            assert_eq!(guard(line), StatementGuard::Allowed, "{line}");
        }
        assert!(matches!(guard("KEYS m7:*"), StatementGuard::Confirm(why) if why.contains("m7:*")));
    }

    #[test]
    fn redaction_replaces_passwords_and_keeps_the_rest() {
        let cases = [
            ("AUTH s3cret", "AUTH ***"),
            ("auth alice s3cret", "auth alice ***"),
            ("AUTH \"pa ss\"", "AUTH ***"),
            ("AUTH a b c", "AUTH *** *** ***"),
            (
                "HELLO 3 AUTH alice s3cret SETNAME x",
                "HELLO 3 AUTH alice *** SETNAME x",
            ),
            (
                "MIGRATE h 6379 \"\" 0 500 COPY AUTH pw KEYS a b",
                "MIGRATE h 6379 \"\" 0 500 COPY AUTH *** KEYS a b",
            ),
            (
                "MIGRATE h 6379 k 0 500 AUTH2 user pw",
                "MIGRATE h 6379 k 0 500 AUTH2 user ***",
            ),
            (
                "ACL SETUSER bob on >pw1 <old #abcd !ef ~* +@all",
                "ACL SETUSER bob on >*** <*** #*** !*** ~* +@all",
            ),
            ("acl setuser bob '>p w'", "acl setuser bob >***"),
            (
                "CONFIG SET requirepass s3cret",
                "CONFIG SET requirepass ***",
            ),
            (
                "CONFIG SET maxmemory 1mb masterauth s3 tls-key-file-pass k",
                "CONFIG SET maxmemory 1mb masterauth *** tls-key-file-pass ***",
            ),
            ("config set MASTERUSER repl", "config set MASTERUSER ***"),
            (
                "SENTINEL SET oxmaster auth-pass s3cret",
                "SENTINEL SET oxmaster auth-pass ***",
            ),
            ("AUTH \"unterminated", "AUTH ***"),
        ];
        for (line, want) in cases {
            assert_eq!(RedisConsole.redact_secrets(line), want, "{line}");
        }
        let multi = "GET a\nAUTH pw\r\nSET b 1\n";
        assert_eq!(
            RedisConsole.redact_secrets(multi),
            "GET a\nAUTH ***\r\nSET b 1\n"
        );
    }

    #[test]
    fn nothing_to_redact_is_borrowed() {
        for text in [
            "",
            "GET a",
            "SET auth s3cret",
            "AUTH",
            "CONFIG GET requirepass",
            "ACL SETUSER bob on ~* +@all",
            "HELLO 3",
            "# AUTH s3cret is a comment? no: a comment line is not a command\n",
        ] {
            let out = RedisConsole.redact_secrets(text);
            assert!(matches!(out, Cow::Borrowed(_)), "{text}");
        }
    }

    #[test]
    fn lexing_matches_the_command_line_reader_on_valid_lines() {
        for line in [
            "SET k v",
            r#"SET "a key" "line\nnext \"q\" \\ \x00\xff\xZZ""#,
            r"SET k 'it\'s \n'",
            "PING ''",
            "GET ü",
        ] {
            let strict = crate::command::parse_command_line(line).unwrap();
            let lenient: Vec<Vec<u8>> = lex(line).into_iter().map(|a| a.value).collect();
            assert_eq!(lenient, strict, "{line}");
        }
    }

    /// Small deterministic generator for the fuzz tests (xorshift64*).
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            let n = u64::try_from(n).unwrap_or(u64::MAX);
            usize::try_from(self.next() % n).unwrap_or(0)
        }
    }

    #[test]
    fn fuzzed_scripts_never_panic_and_keep_the_range_rules() {
        const PIECES: &[&str] = &[
            "AUTH",
            "HELLO",
            "ACL",
            "SETUSER",
            "CONFIG",
            "SET",
            "requirepass",
            "MIGRATE",
            "AUTH2",
            "KEYS",
            "FLUSHDB",
            "GET",
            " ",
            "  ",
            "\t",
            "\n",
            "\r\n",
            "\"",
            "'",
            "\\",
            "\\x",
            "\\xf",
            "\\xff",
            ">",
            "#",
            "//",
            "ü",
            "€",
            "𝄞",
            "a",
            "1",
            "*",
            "SENTINEL",
            "auth-pass",
            "CLIENT",
            "REPLY",
        ];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..5000 {
            let n = rng.below(24);
            let script: String = (0..n).map(|_| PIECES[rng.below(PIECES.len())]).collect();
            let statements = RedisConsole.split(&script);
            let mut last = 0;
            for s in &statements {
                assert!(s.range.start >= last, "{script:?}");
                assert_eq!(&script[s.range.clone()], s.text, "{script:?}");
                assert!(s.segment_end >= s.range.end && s.segment_end <= script.len());
                assert!(!s.text.is_empty());
                last = s.segment_end;
                let _ = RedisConsole.guard(s);
            }
            let redacted = RedisConsole.redact_secrets(&script);
            // Redacting twice changes nothing more.
            assert_eq!(
                RedisConsole.redact_secrets(&redacted),
                redacted,
                "{script:?}"
            );
        }
    }
}
