//! Stand-ins for the `sql` stream's splitter and redaction in this crate's unit tests.
//!
//! The query service is built against `oxbase_sql::split` and `oxbase_sql::redact_secrets`,
//! which another M2 stream implements. Until that lands on a branch, both are
//! `unimplemented!`; the unit tests then split with the minimal splitter below (`;` outside
//! quotes and `--` comments) and redact with the minimal redactor below (a quoted literal
//! after `PASSWORD`, `BY`, `KEY` or `SECRET`), so the run and history machinery can still be
//! tested. Neither is used outside tests. Once the real functions work they are used and this
//! module does nothing; tests that check the real redaction rules belong to `oxbase-sql`.

use std::sync::OnceLock;

use oxbase_sql::{SqlDialect, Statement, StatementKind, position_at};

/// `true` once `oxbase_sql::split` is implemented (it does not panic).
pub(crate) fn real_split_available() -> bool {
    static READY: OnceLock<bool> = OnceLock::new();
    *READY.get_or_init(|| {
        std::panic::catch_unwind(|| oxbase_sql::split(SqlDialect::Postgres, "SELECT 1")).is_ok()
    })
}

/// `true` once `oxbase_sql::redact_secrets` is implemented (it does not panic).
pub(crate) fn redaction_available() -> bool {
    static READY: OnceLock<bool> = OnceLock::new();
    *READY.get_or_init(|| {
        std::panic::catch_unwind(|| {
            oxbase_sql::redact_secrets(SqlDialect::Postgres, "SELECT 1").into_owned()
        })
        .is_ok()
    })
}

/// The minimal redaction of `text`, or `None` when the real `redact_secrets` is available.
pub(crate) fn fallback_redact(text: &str) -> Option<String> {
    if redaction_available() {
        return None;
    }
    Some(simple_redact(text))
}

/// Replaces each `'...'` literal that follows `PASSWORD`, `BY`, `KEY` or `SECRET` (optionally
/// with `=`), case-insensitively, by `'***'`.
pub(crate) fn simple_redact(text: &str) -> String {
    const KEYWORDS: [&str; 4] = ["PASSWORD", "BY", "KEY", "SECRET"];
    let upper = text.to_ascii_uppercase();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while let Some(found) = text[i..].find('\'') {
        let open = i + found;
        let end = text[open + 1..]
            .find('\'')
            .map_or(text.len(), |close| open + 1 + close + 1);
        let before = upper[..open].trim_end();
        let before = before.strip_suffix('=').unwrap_or(before).trim_end();
        if KEYWORDS.iter().any(|k| before.ends_with(k)) {
            out.push_str(&text[copied..open]);
            out.push_str("'***'");
            copied = end;
        }
        i = end;
    }
    out.push_str(&text[copied..]);
    out
}

/// The minimal split of `script`, or `None` when the real splitter is available.
pub(crate) fn fallback_split(dialect: SqlDialect, script: &str) -> Option<Vec<Statement>> {
    let _ = dialect;
    if real_split_available() {
        return None;
    }
    Some(simple_split(script))
}

/// Splits at `;` outside `'...'`, `"..."`, `` `...` `` and `--` line comments.
pub(crate) fn simple_split(script: &str) -> Vec<Statement> {
    let bytes = script.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
        } else {
            match b {
                b'\'' | b'"' | b'`' => quote = Some(b),
                b'-' if bytes.get(i + 1) == Some(&b'-') => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                b';' => {
                    push(script, start, i, i + 1, &mut out);
                    start = i + 1;
                }
                _ => {}
            }
        }
        i += 1;
    }
    push(script, start, script.len(), script.len(), &mut out);
    out
}

fn push(script: &str, from: usize, to: usize, segment_end: usize, out: &mut Vec<Statement>) {
    let segment = &script[from..to];
    let trimmed = segment.trim();
    if trimmed.is_empty() {
        return;
    }
    let start = from + (segment.len() - segment.trim_start().len());
    let end = start + trimmed.len();
    let first = trimmed
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    let kind = if matches!(
        first.as_str(),
        "SELECT" | "WITH" | "VALUES" | "TABLE" | "SHOW" | "PRAGMA" | "EXPLAIN" | "CALL"
    ) {
        StatementKind::Query
    } else {
        StatementKind::Other
    };
    out.push(Statement {
        range: start..end,
        segment_end: segment_end.max(end),
        start: position_at(script, start),
        end: position_at(script, end),
        text: trimmed.to_owned(),
        kind,
    });
}

#[test]
fn simple_split_respects_quotes_and_comments() {
    let s = simple_split("SELECT 'a;b';\n -- c;\nUPDATE t SET x = 1;  ;\nSELECT 2");
    let texts: Vec<&str> = s.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        texts,
        ["SELECT 'a;b'", "-- c;\nUPDATE t SET x = 1", "SELECT 2"]
    );
    assert_eq!(s[0].kind, StatementKind::Query);
    assert_eq!(s[1].kind, StatementKind::Other);
    assert_eq!(s[2].range.end, s[2].segment_end);
}

#[test]
fn simple_redact_hides_literals_after_secret_keywords() {
    assert_eq!(
        simple_redact("ALTER USER x PASSWORD 'p1' VALID UNTIL 'infinity'"),
        "ALTER USER x PASSWORD '***' VALID UNTIL 'infinity'"
    );
    assert_eq!(
        simple_redact("alter user 'u'@'%' identified by 'p2'"),
        "alter user 'u'@'%' identified by '***'"
    );
    assert_eq!(simple_redact("PRAGMA key = 'p3'"), "PRAGMA key = '***'");
    assert_eq!(simple_redact("SELECT 'a', 'b"), "SELECT 'a', 'b");
    assert_eq!(simple_redact("PASSWORD 'unterminated"), "PASSWORD '***'");
}
