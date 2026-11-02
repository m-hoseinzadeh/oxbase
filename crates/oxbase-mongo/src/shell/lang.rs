//! Editor-side rules of the shell language that need no server: statement kinds, guards and
//! history redaction (module docs, *Redaction and guards*). All work on the tokens of
//! [`super::lex`], so text inside strings and comments never triggers a guard.

use std::borrow::Cow;
use std::ops::Range;

use oxbase_dialect::StatementGuard;
use oxbase_sql::StatementKind;
use serde_json::Value as Json;

use super::lex::{Tok, string_content, tokens};
use super::split::request_start;

/// What a redacted secret is replaced by.
const REDACTED: &str = "***";

/// Methods and commands that write (a statement using one is `StatementKind::Other`).
const WRITE_WORDS: &[&str] = &[
    "adminCommand",
    "bulkWrite",
    "createCollection",
    "createIndex",
    "createIndexes",
    "createView",
    "deleteMany",
    "deleteOne",
    "drop",
    "dropDatabase",
    "dropIndex",
    "dropIndexes",
    "findOneAndDelete",
    "findOneAndReplace",
    "findOneAndUpdate",
    "insert",
    "insertMany",
    "insertOne",
    "remove",
    "renameCollection",
    "replaceOne",
    "runCommand",
    "update",
    "updateMany",
    "updateOne",
];

/// Methods that read (a statement using one and no write is `StatementKind::Query`).
const READ_WORDS: &[&str] = &[
    "aggregate",
    "count",
    "countDocuments",
    "distinct",
    "estimatedDocumentCount",
    "find",
    "findOne",
    "getCollectionInfos",
    "getCollectionNames",
    "getIndexes",
    "stats",
    "toArray",
];

const DROP_DATABASE: &str =
    "dropDatabase() deletes the current database with every collection in it";
const DROP: &str = "drop() deletes the collection with its documents and indexes";
const DROP_INDEX: &str = "dropIndex deletes indexes of the collection";
const DELETE_ALL: &str = "deleteMany with an empty filter deletes every document of the collection";
const UPDATE_ALL: &str = "updateMany with an empty filter changes every document of the collection";
const RENAME_DROP: &str =
    "renameCollection(..., true) deletes the target collection when it exists";
const DROP_COMMAND: &str = "the command deletes a database, a collection or indexes";

/// Whether a statement probably returns documents (a hint for the run, which drops a query's
/// stream at its row limit). JSON requests: `find` / `aggregate` are queries.
pub(super) fn statement_kind(text: &str) -> StatementKind {
    if let Some(at) = request_start(text) {
        let kind = serde_json::from_str::<Json>(&text[at..])
            .ok()
            .and_then(|v| v.get("type").and_then(Json::as_str).map(str::to_owned));
        return match kind.as_deref() {
            Some("find" | "aggregate") => StatementKind::Query,
            _ => StatementKind::Other,
        };
    }
    let mut reads = false;
    for (tok, range) in tokens(text) {
        let word = &text[range.clone()];
        match tok {
            Tok::Word if WRITE_WORDS.contains(&word) => return StatementKind::Other,
            Tok::Word if READ_WORDS.contains(&word) => reads = true,
            Tok::Word if word == "$out" || word == "$merge" => return StatementKind::Other,
            Tok::Str => {
                if let Some(content) = string_content(text, tok, &range)
                    && matches!(&text[content], "$out" | "$merge")
                {
                    return StatementKind::Other;
                }
            }
            _ => {}
        }
    }
    if reads {
        StatementKind::Query
    } else {
        StatementKind::Other
    }
}

/// The guard of one statement (see the module docs of `shell`).
pub(super) fn guard(text: &str) -> StatementGuard {
    if let Some(at) = request_start(text) {
        return request_guard(&text[at..]);
    }
    let toks = tokens(text);
    let at = |i: usize| -> Option<(Tok, &str)> {
        let (t, r) = toks.get(i)?;
        Some((*t, &text[r.clone()]))
    };
    let confirm = |why: &str| StatementGuard::Confirm(why.to_owned());
    let mut runs_command = false;
    for (i, (tok, range)) in toks.iter().enumerate() {
        let (tok, word) = (*tok, &text[range.clone()]);
        if tok == Tok::Word && matches!(word, "runCommand" | "adminCommand") {
            runs_command = true;
        }
        // `{dropDatabase: 1}` / `{"drop": "c"}` in a runCommand document.
        if runs_command
            && matches!(at(i + 1), Some((Tok::Punct(b':'), _)))
            && key_text(text, tok, range)
                .is_some_and(|k| matches!(k, "dropDatabase" | "drop" | "dropIndexes"))
        {
            return confirm(DROP_COMMAND);
        }
        if runs_command
            && key_text(text, tok, range) == Some("dropTarget")
            && matches!(at(i + 1), Some((Tok::Punct(b':'), _)))
            && matches!(at(i + 2), Some((Tok::Word, "true")))
        {
            return confirm(RENAME_DROP);
        }
        // `.method(`
        if tok != Tok::Punct(b'.') {
            continue;
        }
        let (Some((Tok::Word, method)), Some((Tok::Punct(b'('), _))) = (at(i + 1), at(i + 2))
        else {
            continue;
        };
        let args = i + 3;
        let empty_filter = |follow: &[u8]| match at(args) {
            Some((Tok::Punct(b')'), _)) => follow.contains(&b')'),
            Some((Tok::Punct(b'{'), _)) => {
                matches!(at(args + 1), Some((Tok::Punct(b'}'), _)))
                    && matches!(at(args + 2), Some((Tok::Punct(c), _)) if follow.contains(&c))
            }
            _ => false,
        };
        match method {
            "dropDatabase" => return confirm(DROP_DATABASE),
            "drop" => return confirm(DROP),
            "dropIndex" | "dropIndexes" => return confirm(DROP_INDEX),
            "deleteMany" if empty_filter(b"),") => return confirm(DELETE_ALL),
            "updateMany" if empty_filter(b",") => return confirm(UPDATE_ALL),
            "renameCollection" if second_argument_is_true(&toks, text, args) => {
                return confirm(RENAME_DROP);
            }
            _ => {}
        }
    }
    StatementGuard::Allowed
}

/// The text of a key token: a word, or a string's content.
fn key_text<'t>(text: &'t str, tok: Tok, range: &Range<usize>) -> Option<&'t str> {
    match tok {
        Tok::Word => Some(&text[range.clone()]),
        Tok::Str => string_content(text, tok, range).map(|r| &text[r]),
        _ => None,
    }
}

/// Whether the call whose arguments start at token `args` has a second argument `true`.
fn second_argument_is_true(toks: &[(Tok, Range<usize>)], text: &str, args: usize) -> bool {
    let mut depth = 0usize;
    let mut i = args;
    while let Some((tok, _)) = toks.get(i) {
        match tok {
            Tok::Punct(b'(' | b'[' | b'{') => depth += 1,
            Tok::Punct(b')' | b']' | b'}') => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            Tok::Punct(b',') if depth == 0 => {
                return matches!(toks.get(i + 1), Some((Tok::Word, r)) if &text[r.clone()] == "true")
                    && matches!(toks.get(i + 2), Some((Tok::Punct(b')' | b','), _)));
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// The guard of a JSON request statement: a command request that drops something.
fn request_guard(text: &str) -> StatementGuard {
    let Ok(value) = serde_json::from_str::<Json>(text) else {
        return StatementGuard::Allowed;
    };
    if value.get("type").and_then(Json::as_str) != Some("command") {
        return StatementGuard::Allowed;
    }
    let Some(command) = value.get("command").and_then(Json::as_object) else {
        return StatementGuard::Allowed;
    };
    match command.keys().next().map(String::as_str) {
        Some("dropDatabase" | "drop" | "dropIndexes") => {
            StatementGuard::Confirm(DROP_COMMAND.to_owned())
        }
        Some("renameCollection") if command.get("dropTarget") == Some(&Json::Bool(true)) => {
            StatementGuard::Confirm(RENAME_DROP.to_owned())
        }
        _ => StatementGuard::Allowed,
    }
}

/// `text` with passwords replaced (see the module docs of `shell`).
pub(super) fn redact(text: &str) -> Cow<'_, str> {
    let toks = tokens(text);
    let mut edits: Vec<Range<usize>> = Vec::new();
    let content = |i: usize| -> Option<Range<usize>> {
        toks.get(i).and_then(|(t, r)| string_content(text, *t, r))
    };
    let word = |i: usize| -> Option<&str> {
        toks.get(i)
            .filter(|(t, _)| *t == Tok::Word)
            .map(|(_, r)| &text[r.clone()])
    };
    let punct = |i: usize, c: u8| matches!(toks.get(i), Some((Tok::Punct(p), _)) if *p == c);
    for (i, (tok, range)) in toks.iter().enumerate() {
        // pwd: "..." / "password": '...'
        if key_text(text, *tok, range).is_some_and(|k| matches!(k, "pwd" | "password"))
            && punct(i + 1, b':')
            && let Some(secret) = content(i + 2)
        {
            edits.push(secret);
        }
        // changeUserPassword("user", "secret") / db.auth("user", "secret")
        let call = match word(i) {
            Some("changeUserPassword") => true,
            Some("auth") => i > 0 && punct(i - 1, b'.'),
            _ => false,
        };
        if call
            && punct(i + 1, b'(')
            && content(i + 2).is_some()
            && punct(i + 3, b',')
            && let Some(secret) = content(i + 4)
        {
            edits.push(secret);
        }
        // mongodb://user:secret@host
        if let Some(inside) = string_content(text, *tok, range) {
            uri_passwords(text, inside, &mut edits);
        }
    }
    edits.retain(|r| !r.is_empty() && &text[r.clone()] != REDACTED);
    if edits.is_empty() {
        return Cow::Borrowed(text);
    }
    edits.sort_by_key(|r| r.start);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for edit in edits {
        if edit.start < at {
            continue;
        }
        out.push_str(&text[at..edit.start]);
        out.push_str(REDACTED);
        at = edit.end;
    }
    out.push_str(&text[at..]);
    Cow::Owned(out)
}

/// The password ranges of `mongodb://user:password@` URIs inside `range` of `text`.
fn uri_passwords(text: &str, range: Range<usize>, edits: &mut Vec<Range<usize>>) {
    let part = &text[range.clone()];
    let lower = part.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find("mongodb") {
        let scheme = from + found;
        let rest = &lower[scheme..];
        let after = if rest.starts_with("mongodb://") {
            scheme + "mongodb://".len()
        } else if rest.starts_with("mongodb+srv://") {
            scheme + "mongodb+srv://".len()
        } else {
            from = scheme + "mongodb".len();
            continue;
        };
        let bytes = part.as_bytes();
        let mut j = after;
        while j < bytes.len() && !b"@/?#\"'` \t\r\n".contains(&bytes[j]) {
            j += 1;
        }
        if j < bytes.len()
            && bytes[j] == b'@'
            && let Some(colon) = part[after..j].find(':')
        {
            let start = range.start + after + colon + 1;
            edits.push(start..range.start + j);
        }
        from = j.max(after);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn redaction_table() {
        let cases = [
            (
                r#"db.createUser({user: "u", pwd: "s3cret", roles: []})"#,
                r#"db.createUser({user: "u", pwd: "***", roles: []})"#,
            ),
            (
                r#"db.runCommand({createUser: 'u', "pwd": 's3cret'})"#,
                r#"db.runCommand({createUser: 'u', "pwd": '***'})"#,
            ),
            (
                r#"db.updateUser("u", {password : `s3cret`})"#,
                r#"db.updateUser("u", {password : `***`})"#,
            ),
            (
                r#"db.changeUserPassword("u", "n3w")"#,
                r#"db.changeUserPassword("u", "***")"#,
            ),
            (r#"db.auth("u", "pw")"#, r#"db.auth("u", "***")"#),
            (
                r#"db.auth({user: "u", pwd: "pw", mechanism: "SCRAM-SHA-256"})"#,
                r#"db.auth({user: "u", pwd: "***", mechanism: "SCRAM-SHA-256"})"#,
            ),
            (
                r#"x = "mongodb://alice:pa55@h:27017/db?x=1""#,
                r#"x = "mongodb://alice:***@h:27017/db?x=1""#,
            ),
            (
                r#"connect('MONGODB+SRV://a:b@c.example/d', "mongodb://e:f@g")"#,
                r#"connect('MONGODB+SRV://a:***@c.example/d', "mongodb://e:***@g")"#,
            ),
            (
                "db.createUser({user: 'a', pwd: 'x'})\ndb.createUser({user: 'b', pwd: 'y'})",
                "db.createUser({user: 'a', pwd: '***'})\ndb.createUser({user: 'b', pwd: '***'})",
            ),
            (
                r#"db.createUser({user: "u", pwd: "unterminated"#,
                r#"db.createUser({user: "u", pwd: "***"#,
            ),
        ];
        for (text, want) in cases {
            assert_eq!(redact(text), want, "{text}");
            // Redacting the result changes nothing more.
            assert!(matches!(redact(want), Cow::Borrowed(_)), "{want}");
        }
    }

    #[test]
    fn nothing_to_redact_is_borrowed() {
        for text in [
            "",
            "db.c.find({pwd: 1})",
            "db.c.find({pwd: passwordPrompt()})",
            "// pwd: 'in a comment'",
            "x = 'pwd: secret'",
            "x = 'mongodb://host:27017/db'",
            "x = 'mongodb://user@host'",
            "db.c.insertOne({password: ''})",
            "auth('a', 'b')",
            r#"db.createUser({user: "u", pwd: "***"})"#,
        ] {
            assert!(matches!(redact(text), Cow::Borrowed(_)), "{text}");
        }
    }

    #[test]
    fn guard_table() {
        let confirm = [
            "db.dropDatabase()",
            "db.getSiblingDB('x').dropDatabase( )",
            "db.c.drop()",
            "db.getCollection('c').drop()",
            "db.c.dropIndex('a_1')",
            "db.c.dropIndexes()",
            "db.c.deleteMany({})",
            "db.c.deleteMany()",
            "db.c.deleteMany( { } , {w: 1})",
            "db.c.updateMany({}, {$set: {a: 1}})",
            "db.c.renameCollection('d', true)",
            "db.runCommand({drop: 'c'})",
            "db.adminCommand({\"dropDatabase\": 1})",
            "db.adminCommand({renameCollection: 'a.b', to: 'a.c', dropTarget: true})",
            r#"{"type": "command", "database": "d", "command": {"drop": "c"}}"#,
            r#"{"type": "command", "database": "admin", "command": {"renameCollection": "a.b", "to": "a.c", "dropTarget": true}}"#,
        ];
        for text in confirm {
            assert!(
                matches!(guard(text), StatementGuard::Confirm(_)),
                "{text}: {:?}",
                guard(text)
            );
        }
        let allowed = [
            "db.c.find({drop: 1})",
            "db.c.deleteMany({a: 1})",
            "db.c.deleteOne({})",
            "db.c.updateMany({a: 1}, {$set: {b: 1}})",
            "db.c.updateOne({}, {$set: {b: 1}})",
            "db.c.renameCollection('d')",
            "db.c.renameCollection('d', false)",
            "print('db.dropDatabase()')",
            "// db.c.drop()",
            "db.c.insertOne({drop: true})",
            "db.dropped.find()",
            r#"{"type": "find", "collection": {"kind": "table", "catalog": "d", "name": "c"}}"#,
            r#"{"type": "command", "database": "admin", "command": {"ping": 1}}"#,
            "{ not json",
        ];
        for text in allowed {
            assert_eq!(guard(text), StatementGuard::Allowed, "{text}");
        }
    }

    #[test]
    fn kinds() {
        let cases = [
            ("db.c.find()", StatementKind::Query),
            ("db.c.aggregate([{$match: {}}])", StatementKind::Query),
            ("db.c.aggregate([{$out: 'x'}])", StatementKind::Other),
            ("db.c.aggregate([{'$merge': 'x'}])", StatementKind::Other),
            ("db.c.countDocuments({})", StatementKind::Query),
            ("db.c.insertOne({})", StatementKind::Other),
            (
                "db.c.find().forEach(d => db.d.insertOne(d))",
                StatementKind::Other,
            ),
            ("print(1)", StatementKind::Other),
            (r#"{"type": "aggregate"}"#, StatementKind::Query),
            (r#"{"type": "command"}"#, StatementKind::Other),
        ];
        for (text, kind) in cases {
            assert_eq!(statement_kind(text), kind, "{text}");
        }
    }
}
