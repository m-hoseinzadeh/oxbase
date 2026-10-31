//! A small reader for the `CREATE` statements SQLite keeps in `sqlite_schema.sql`.
//!
//! SQLite's catalog pragmas do not report everything the object model needs: constraint names,
//! check expressions, generated-column expressions, column collations, `AUTOINCREMENT`, index
//! key expressions and predicates, view bodies and trigger clauses only exist in the stored
//! `CREATE` text. This module tokenizes that text (SQLite's lexical rules: `'...'` strings,
//! `"..."`, `[...]` and `` `...` `` identifiers, `--` and `/* */` comments) and picks those parts
//! out. It never evaluates or rewrites SQL: every expression is a slice of the original text.
//!
//! The parsers are lenient: text they do not understand yields fewer facts, never an error, and
//! the facts the pragmas report stay authoritative.

/// Token classes the parsers care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    /// A bare word (keyword or unquoted identifier).
    Word,
    /// A quoted identifier: `"x"`, `[x]` or `` `x` ``.
    Quoted,
    /// A string literal `'x'` (also blob literals `X'..'`).
    Str,
    Num,
    LParen,
    RParen,
    Comma,
    Dot,
    /// Any other punctuation or operator character.
    Other,
}

#[derive(Debug, Clone, Copy)]
struct Token {
    kind: Tok,
    start: usize,
    end: usize,
}

/// Tokenized SQL text.
struct Lexed<'a> {
    sql: &'a str,
    toks: Vec<Token>,
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

/// Byte index just past the closing `close` of a quoted run starting at `start` (the opening
/// character). `doubled` means a doubled `close` is an escaped one. Unterminated runs end at the
/// end of the text.
fn quoted_end(sql: &str, start: usize, open_len: usize, close: char, doubled: bool) -> usize {
    let bytes_after = start + open_len;
    let mut iter = sql[bytes_after..].char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        if c == close {
            if doubled && iter.peek().is_some_and(|&(_, n)| n == close) {
                iter.next();
                continue;
            }
            return bytes_after + i + c.len_utf8();
        }
    }
    sql.len()
}

fn lex(sql: &str) -> Lexed<'_> {
    let mut toks = Vec::new();
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < sql.len() {
        let Some(c) = sql[i..].chars().next() else {
            break;
        };
        let next = sql[i + c.len_utf8()..].chars().next();
        let (kind, end) = match c {
            c if c.is_whitespace() => {
                i += c.len_utf8();
                continue;
            }
            '-' if next == Some('-') => {
                i = sql[i..].find('\n').map_or(sql.len(), |n| i + n + 1);
                continue;
            }
            '/' if next == Some('*') => {
                i = sql[i + 2..].find("*/").map_or(sql.len(), |n| i + 2 + n + 2);
                continue;
            }
            '\'' => (Tok::Str, quoted_end(sql, i, 1, '\'', true)),
            '"' => (Tok::Quoted, quoted_end(sql, i, 1, '"', true)),
            '`' => (Tok::Quoted, quoted_end(sql, i, 1, '`', true)),
            '[' => (Tok::Quoted, quoted_end(sql, i, 1, ']', false)),
            'x' | 'X' if next == Some('\'') => (Tok::Str, quoted_end(sql, i, 2, '\'', true)),
            '(' => (Tok::LParen, i + 1),
            ')' => (Tok::RParen, i + 1),
            ',' => (Tok::Comma, i + 1),
            '.' if !next.is_some_and(|n| n.is_ascii_digit()) => (Tok::Dot, i + 1),
            c if c.is_ascii_digit() || c == '.' => {
                let mut j = i + 1;
                while j < bytes.len() {
                    let b = bytes[j];
                    let exp_sign = (b == b'+' || b == b'-')
                        && matches!(bytes[j - 1], b'e' | b'E')
                        && !sql[i..j].starts_with("0x")
                        && !sql[i..j].starts_with("0X");
                    if b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || exp_sign {
                        j += 1;
                    } else {
                        break;
                    }
                }
                (Tok::Num, j)
            }
            c if is_ident_start(c) => {
                let len = sql[i..]
                    .char_indices()
                    .find(|&(_, ch)| !is_ident_char(ch))
                    .map_or(sql.len() - i, |(n, _)| n);
                (Tok::Word, i + len)
            }
            c => (Tok::Other, i + c.len_utf8()),
        };
        toks.push(Token {
            kind,
            start: i,
            end,
        });
        i = end;
    }
    Lexed { sql, toks }
}

/// The value of an identifier written in SQL: quotes removed and doubled quotes undone. String
/// literals count too (SQLite accepts `'name'` where it expects a name).
pub(crate) fn unquote(text: &str) -> String {
    // Every quote character is ASCII, so the first and last bytes decide; slicing one byte off
    // each end is then always on a character boundary. A text of one multi-byte character (an
    // unquoted `名`) is longer than one byte but never quoted.
    let bytes = text.as_bytes();
    let [first, .., last] = bytes else {
        return text.to_owned();
    };
    let inner = || &text[1..text.len() - 1];
    match (*first, *last) {
        (b'"', b'"') => inner().replace("\"\"", "\""),
        (b'`', b'`') => inner().replace("``", "`"),
        (b'\'', b'\'') => inner().replace("''", "'"),
        (b'[', b']') => inner().to_owned(),
        _ => text.to_owned(),
    }
}

impl Lexed<'_> {
    fn text(&self, i: usize) -> &str {
        self.toks.get(i).map_or("", |t| &self.sql[t.start..t.end])
    }

    fn kind(&self, i: usize) -> Option<Tok> {
        self.toks.get(i).map(|t| t.kind)
    }

    /// `true` when token `i` is the bare word `kw` (ASCII case-insensitive).
    fn is(&self, i: usize, kw: &str) -> bool {
        self.kind(i) == Some(Tok::Word) && self.text(i).eq_ignore_ascii_case(kw)
    }

    fn is_any(&self, i: usize, kws: &[&str]) -> bool {
        kws.iter().any(|kw| self.is(i, kw))
    }

    /// `true` when token `i` can be a name (a word, a quoted identifier or a string).
    fn is_name(&self, i: usize) -> bool {
        matches!(self.kind(i), Some(Tok::Word | Tok::Quoted | Tok::Str))
    }

    fn name(&self, i: usize) -> String {
        unquote(self.text(i))
    }

    /// Index of the `)` matching the `(` at `open`, or the token count when unterminated.
    fn close_of(&self, open: usize) -> usize {
        let mut depth = 0usize;
        for (i, t) in self.toks.iter().enumerate().skip(open) {
            match t.kind {
                Tok::LParen => depth += 1,
                Tok::RParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return i;
                    }
                }
                _ => {}
            }
        }
        self.toks.len()
    }

    /// Source text from the start of token `from` to the end of token `to - 1` (empty when the
    /// range is empty).
    fn slice(&self, from: usize, to: usize) -> &str {
        if from >= to || from >= self.toks.len() {
            return "";
        }
        let end = self.toks[to.min(self.toks.len()) - 1].end;
        &self.sql[self.toks[from].start..end]
    }

    /// Source text from the start of token `from` to the end of the SQL, trimmed, without a
    /// trailing `;`.
    fn rest(&self, from: usize) -> &str {
        let Some(t) = self.toks.get(from) else {
            return "";
        };
        self.sql[t.start..].trim().trim_end_matches(';').trim_end()
    }

    /// The text inside the parenthesized group opening at `open` (trimmed), and the index after
    /// its `)`.
    fn group(&self, open: usize) -> (&str, usize) {
        let close = self.close_of(open);
        (self.slice(open + 1, close).trim(), close + 1)
    }

    /// Ranges of the comma-separated items between `from` and `to` (top level only).
    fn items(&self, from: usize, to: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut depth = 0usize;
        let mut start = from;
        let to = to.min(self.toks.len());
        for (i, t) in self.toks.iter().enumerate().take(to).skip(from) {
            match t.kind {
                Tok::LParen => depth += 1,
                Tok::RParen => depth = depth.saturating_sub(1),
                Tok::Comma if depth == 0 => {
                    out.push((start, i));
                    start = i + 1;
                }
                _ => {}
            }
        }
        if start < to {
            out.push((start, to));
        }
        out
    }

    /// Names of a parenthesized name list at `open` (`(a, "b", c COLLATE x DESC)`): the first
    /// token of each item.
    fn name_list(&self, open: usize) -> (Vec<String>, usize) {
        let close = self.close_of(open);
        let names = self
            .items(open + 1, close)
            .into_iter()
            .filter(|&(a, _)| self.is_name(a))
            .map(|(a, _)| self.name(a))
            .collect();
        (names, close + 1)
    }

    /// Skips `CREATE [TEMP|TEMPORARY] [UNIQUE] [VIRTUAL] <what> [IF NOT EXISTS]`, returning the
    /// index of the object name, or `None` when the text does not start that way.
    fn after_create(&self, what: &str) -> Option<usize> {
        if !self.is(0, "CREATE") {
            return None;
        }
        let mut i = 1;
        while self.is_any(i, &["TEMP", "TEMPORARY", "UNIQUE", "VIRTUAL"]) {
            i += 1;
        }
        if !self.is(i, what) {
            return None;
        }
        i += 1;
        if self.is(i, "IF") && self.is(i + 1, "NOT") && self.is(i + 2, "EXISTS") {
            i += 3;
        }
        Some(i)
    }

    /// Skips a possibly schema-qualified name at `i`, returning the index after it.
    fn skip_name(&self, i: usize) -> usize {
        if self.kind(i + 1) == Some(Tok::Dot) && self.is_name(i + 2) {
            i + 3
        } else {
            i + 1
        }
    }
}

/// What the stored `CREATE TABLE` text says beyond the pragmas.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ParsedTable {
    /// Column definitions in declaration order.
    pub columns: Vec<ParsedColumn>,
    /// `CONSTRAINT <name>` of the primary key, if named.
    pub pk_name: Option<String>,
    /// Checks in declaration order (column checks where their column is declared).
    pub checks: Vec<ParsedCheck>,
    /// Foreign keys in declaration order.
    pub foreign_keys: Vec<ParsedForeignKey>,
    pub without_rowid: bool,
    pub strict: bool,
    /// `CREATE VIRTUAL TABLE ... USING <module>`.
    pub module: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ParsedColumn {
    pub name: String,
    pub collation: Option<String>,
    /// `[GENERATED ALWAYS] AS (<expression>)`.
    pub generated: Option<String>,
    /// `PRIMARY KEY ... AUTOINCREMENT`.
    pub autoincrement: bool,
    /// A column-level `PRIMARY KEY DESC`: SQLite does not make such an `INTEGER` column a rowid
    /// alias (a table-level `PRIMARY KEY (x DESC)` still is one).
    pub pk_desc: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedCheck {
    pub name: Option<String>,
    /// The expression inside `CHECK ( ... )`, as written.
    pub expression: String,
    /// The column a column-level check is declared on.
    pub column: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ParsedForeignKey {
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub deferrable: bool,
    pub initially_deferred: bool,
}

const TABLE_CONSTRAINT_START: &[&str] = &["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"];

/// Reads a stored `CREATE TABLE` / `CREATE VIRTUAL TABLE` statement.
pub(crate) fn parse_create_table(sql: &str) -> ParsedTable {
    let lx = lex(sql);
    let mut out = ParsedTable::default();
    let Some(name_at) = lx.after_create("TABLE") else {
        return out;
    };
    let mut i = lx.skip_name(name_at);
    if lx.is(i, "USING") {
        if lx.is_name(i + 1) {
            out.module = Some(lx.name(i + 1));
        }
        return out;
    }
    if lx.kind(i) != Some(Tok::LParen) {
        // `CREATE TABLE ... AS SELECT`: no declared constraints.
        return out;
    }
    let close = lx.close_of(i);
    for (a, b) in lx.items(i + 1, close) {
        if lx.is_any(a, TABLE_CONSTRAINT_START) {
            table_constraints(&lx, a, b, &mut out);
        } else if lx.is_name(a) {
            column_def(&lx, a, b, &mut out);
        }
    }
    // Table options: `WITHOUT ROWID`, `STRICT`, comma separated.
    i = close + 1;
    while i < lx.toks.len() {
        if lx.is(i, "WITHOUT") && lx.is(i + 1, "ROWID") {
            out.without_rowid = true;
            i += 1;
        } else if lx.is(i, "STRICT") {
            out.strict = true;
        }
        i += 1;
    }
    out
}

/// Words that start a column constraint.
const COLUMN_CONSTRAINT_START: &[&str] = &[
    "CONSTRAINT",
    "PRIMARY",
    "NOT",
    "NULL",
    "UNIQUE",
    "CHECK",
    "DEFAULT",
    "COLLATE",
    "REFERENCES",
    "GENERATED",
    "AS",
];

fn column_def(lx: &Lexed<'_>, a: usize, b: usize, out: &mut ParsedTable) {
    let mut col = ParsedColumn {
        name: lx.name(a),
        ..ParsedColumn::default()
    };
    let mut i = a + 1;
    // The type: everything up to the first constraint word.
    while i < b && !lx.is_any(i, COLUMN_CONSTRAINT_START) {
        i = if lx.kind(i) == Some(Tok::LParen) {
            lx.close_of(i) + 1
        } else {
            i + 1
        };
    }
    let mut pending: Option<String> = None;
    while i < b {
        if lx.is(i, "CONSTRAINT") {
            pending = lx.is_name(i + 1).then(|| lx.name(i + 1));
            i += 2;
        } else if lx.is(i, "PRIMARY") {
            out.pk_name = pending.take();
            if lx.is(i + 1, "KEY") && lx.is(i + 2, "DESC") {
                col.pk_desc = true;
            }
            i += 1;
        } else if lx.is(i, "AUTOINCREMENT") {
            col.autoincrement = true;
            i += 1;
        } else if lx.is(i, "CHECK") && lx.kind(i + 1) == Some(Tok::LParen) {
            let (expr, next) = lx.group(i + 1);
            out.checks.push(ParsedCheck {
                name: pending.take(),
                expression: expr.to_owned(),
                column: Some(col.name.clone()),
            });
            i = next;
        } else if lx.is(i, "COLLATE") && lx.is_name(i + 1) {
            pending = None;
            col.collation = Some(lx.name(i + 1));
            i += 2;
        } else if lx.is(i, "AS") && lx.kind(i + 1) == Some(Tok::LParen) {
            pending = None;
            let (expr, next) = lx.group(i + 1);
            col.generated = Some(expr.to_owned());
            i = next;
        } else if lx.is(i, "REFERENCES") {
            let mut fk = ParsedForeignKey {
                name: pending.take(),
                columns: vec![col.name.clone()],
                ..ParsedForeignKey::default()
            };
            i = references(lx, i + 1, b, &mut fk);
            out.foreign_keys.push(fk);
        } else if lx.kind(i) == Some(Tok::LParen) {
            // A `DEFAULT (expr)` or another group: never looked into.
            i = lx.close_of(i) + 1;
        } else {
            if lx.is_any(i, COLUMN_CONSTRAINT_START) {
                pending = None;
            }
            i += 1;
        }
    }
    out.columns.push(col);
}

fn table_constraints(lx: &Lexed<'_>, a: usize, b: usize, out: &mut ParsedTable) {
    let mut pending: Option<String> = None;
    let mut i = a;
    while i < b {
        if lx.is(i, "CONSTRAINT") {
            pending = lx.is_name(i + 1).then(|| lx.name(i + 1));
            i += 2;
        } else if lx.is(i, "PRIMARY") {
            out.pk_name = pending.take();
            i += 1;
        } else if lx.is(i, "CHECK") && lx.kind(i + 1) == Some(Tok::LParen) {
            let (expr, next) = lx.group(i + 1);
            out.checks.push(ParsedCheck {
                name: pending.take(),
                expression: expr.to_owned(),
                column: None,
            });
            i = next;
        } else if lx.is(i, "FOREIGN") && lx.is(i + 1, "KEY") && lx.kind(i + 2) == Some(Tok::LParen)
        {
            let (columns, next) = lx.name_list(i + 2);
            let mut fk = ParsedForeignKey {
                name: pending.take(),
                columns,
                ..ParsedForeignKey::default()
            };
            i = next;
            if lx.is(i, "REFERENCES") {
                i = references(lx, i + 1, b, &mut fk);
            }
            out.foreign_keys.push(fk);
        } else if lx.kind(i) == Some(Tok::LParen) {
            i = lx.close_of(i) + 1;
        } else {
            if lx.is_any(i, TABLE_CONSTRAINT_START) {
                pending = None;
            }
            i += 1;
        }
    }
}

/// Reads `<table> [(cols)] [ON DELETE|UPDATE action | MATCH name]* [[NOT] DEFERRABLE
/// [INITIALLY DEFERRED|IMMEDIATE]]` starting at `i` (just after `REFERENCES`).
fn references(lx: &Lexed<'_>, mut i: usize, b: usize, fk: &mut ParsedForeignKey) -> usize {
    if lx.is_name(i) {
        fk.ref_table = lx.name(i);
        i += 1;
    }
    if lx.kind(i) == Some(Tok::LParen) {
        let (cols, next) = lx.name_list(i);
        fk.ref_columns = cols;
        i = next;
    }
    while i < b {
        if lx.is(i, "ON") {
            // ON DELETE|UPDATE SET NULL|SET DEFAULT|CASCADE|RESTRICT|NO ACTION
            i += 2;
            i += if lx.is_any(i, &["SET", "NO"]) { 2 } else { 1 };
        } else if lx.is(i, "MATCH") {
            i += 2;
        } else if lx.is(i, "NOT") && lx.is(i + 1, "DEFERRABLE") {
            fk.deferrable = false;
            i += 2;
        } else if lx.is(i, "DEFERRABLE") {
            fk.deferrable = true;
            i += 1;
        } else if lx.is(i, "INITIALLY") {
            fk.initially_deferred = lx.is(i + 1, "DEFERRED");
            i += 2;
        } else {
            break;
        }
    }
    i
}

/// What the stored `CREATE INDEX` text says beyond the pragmas.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ParsedIndex {
    /// Each key part as written, without a trailing `COLLATE x` / `ASC` / `DESC`.
    pub parts: Vec<String>,
    /// The partial-index predicate after `WHERE`.
    pub predicate: Option<String>,
}

/// Reads a stored `CREATE [UNIQUE] INDEX` statement.
pub(crate) fn parse_create_index(sql: &str) -> ParsedIndex {
    let lx = lex(sql);
    let mut out = ParsedIndex::default();
    let Some(name_at) = lx.after_create("INDEX") else {
        return out;
    };
    let i = lx.skip_name(name_at);
    if !lx.is(i, "ON") {
        return out;
    }
    let open = i + 2;
    if lx.kind(open) != Some(Tok::LParen) {
        return out;
    }
    let close = lx.close_of(open);
    for (a, b) in lx.items(open + 1, close) {
        let mut end = b;
        loop {
            if end > a + 1 && lx.is_any(end - 1, &["ASC", "DESC"]) {
                end -= 1;
            } else if end > a + 2 && lx.is(end - 2, "COLLATE") {
                end -= 2;
            } else {
                break;
            }
        }
        out.parts.push(lx.slice(a, end).trim().to_owned());
    }
    if lx.is(close + 1, "WHERE") {
        out.predicate = Some(lx.rest(close + 2).to_owned()).filter(|p| !p.is_empty());
    }
    out
}

/// What the stored `CREATE VIEW` text says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ParsedView {
    /// The declared column list (`CREATE VIEW v(a, b) AS ...`), empty when none.
    pub columns: Vec<String>,
    /// The query after `AS`.
    pub definition: String,
}

/// Reads a stored `CREATE VIEW` statement.
pub(crate) fn parse_create_view(sql: &str) -> ParsedView {
    let lx = lex(sql);
    let mut out = ParsedView::default();
    let Some(name_at) = lx.after_create("VIEW") else {
        return out;
    };
    let mut i = lx.skip_name(name_at);
    if lx.kind(i) == Some(Tok::LParen) {
        let (cols, next) = lx.name_list(i);
        out.columns = cols;
        i = next;
    }
    if lx.is(i, "AS") {
        lx.rest(i + 1).clone_into(&mut out.definition);
    }
    out
}

/// When a trigger fires, as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParsedTiming {
    Before,
    After,
    InsteadOf,
}

/// The event a trigger fires on (SQLite triggers have exactly one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParsedEvent {
    Insert,
    Update,
    Delete,
}

/// What the stored `CREATE TRIGGER` text says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedTrigger {
    /// `BEFORE` when the statement names no timing (SQLite's default).
    pub timing: ParsedTiming,
    pub event: ParsedEvent,
    /// `UPDATE OF a, b` columns.
    pub update_columns: Vec<String>,
    /// The database written in the `ON` clause (`ON main.t`), if any. Only `TEMP` triggers keep
    /// it: SQLite ignores it for a trigger stored in the table's own database.
    pub table_schema: Option<String>,
    /// The `WHEN` expression without the keyword and without one pair of enclosing parentheses.
    pub condition: Option<String>,
    /// `BEGIN ... END`.
    pub body: String,
}

/// Words a trigger body's first statement can start with.
const TRIGGER_STATEMENT_START: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "REPLACE", "WITH", "VALUES",
];

/// Index of the `BEGIN` that opens a trigger body, searching from `from`. `begin` is also a valid
/// unquoted column name (`WHEN NEW.begin > 0`), so only a `BEGIN` outside parentheses and not
/// after a `.` counts, preferably one followed by a statement keyword.
fn body_start(lx: &Lexed<'_>, from: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut fallback = None;
    for j in from..lx.toks.len() {
        match lx.kind(j) {
            Some(Tok::LParen) => depth += 1,
            Some(Tok::RParen) => depth = depth.saturating_sub(1),
            _ if depth == 0
                && lx.is(j, "BEGIN")
                && (j == 0 || lx.kind(j - 1) != Some(Tok::Dot)) =>
            {
                if lx.is_any(j + 1, TRIGGER_STATEMENT_START) {
                    return Some(j);
                }
                fallback = fallback.or(Some(j));
            }
            _ => {}
        }
    }
    fallback
}

/// Reads a stored `CREATE TRIGGER` statement; `None` when it does not look like one.
pub(crate) fn parse_create_trigger(sql: &str) -> Option<ParsedTrigger> {
    let lx = lex(sql);
    let name_at = lx.after_create("TRIGGER")?;
    let mut i = lx.skip_name(name_at);
    let timing = if lx.is(i, "BEFORE") {
        i += 1;
        ParsedTiming::Before
    } else if lx.is(i, "AFTER") {
        i += 1;
        ParsedTiming::After
    } else if lx.is(i, "INSTEAD") && lx.is(i + 1, "OF") {
        i += 2;
        ParsedTiming::InsteadOf
    } else {
        ParsedTiming::Before
    };
    let mut update_columns = Vec::new();
    let event = if lx.is(i, "INSERT") {
        i += 1;
        ParsedEvent::Insert
    } else if lx.is(i, "DELETE") {
        i += 1;
        ParsedEvent::Delete
    } else if lx.is(i, "UPDATE") {
        i += 1;
        if lx.is(i, "OF") {
            i += 1;
            while lx.is_name(i) && !lx.is(i, "ON") {
                update_columns.push(lx.name(i));
                i += 1;
                if lx.kind(i) == Some(Tok::Comma) {
                    i += 1;
                }
            }
        }
        ParsedEvent::Update
    } else {
        return None;
    };
    if !lx.is(i, "ON") {
        return None;
    }
    let table_schema =
        (lx.kind(i + 2) == Some(Tok::Dot) && lx.is_name(i + 3)).then(|| lx.name(i + 1));
    i = lx.skip_name(i + 1);
    if lx.is(i, "FOR") && lx.is(i + 1, "EACH") && lx.is(i + 2, "ROW") {
        i += 3;
    }
    let begin = body_start(&lx, i)?;
    let condition = lx.is(i, "WHEN").then(|| {
        let mut text = lx.slice(i + 1, begin).trim();
        if lx.kind(i + 1) == Some(Tok::LParen) && lx.close_of(i + 1) + 1 == begin {
            text = lx.group(i + 1).0;
        }
        text.to_owned()
    });
    Some(ParsedTrigger {
        timing,
        event,
        update_columns,
        table_schema,
        condition,
        body: lx.rest(begin).to_owned(),
    })
}

// --- M3: rewriting stored text for the table rebuild ---------------------------------------

/// `true` when `sql` names `ident`: a bare word or a quoted identifier equal to it (ASCII
/// case-insensitive, as SQLite compares names). String literals do not count. Used to decide
/// whether a column is referenced by an expression or a trigger (a conservative, lexical test).
pub(crate) fn mentions_identifier(sql: &str, ident: &str) -> bool {
    let lx = lex(sql);
    (0..lx.toks.len()).any(|i| {
        matches!(lx.kind(i), Some(Tok::Word | Tok::Quoted))
            && lx.name(i).eq_ignore_ascii_case(ident)
    })
}

/// A stored `CREATE TRIGGER` statement with renamed objects replaced, for re-creating the trigger
/// after a table rebuild whose script renamed the table or columns first (the rename rewrote the
/// stored text on the server, but the rebuild re-creates the trigger from the text loaded
/// before).
///
/// Every bare or quoted identifier (not the trigger's own name, not a string literal) equal to an
/// old name (ASCII case-insensitive) becomes `quote(new)`: a name followed by `.` is a table
/// qualifier and only matches `table`; other names match `columns` first, then `table`. `None`
/// when nothing was replaced.
pub(crate) fn rename_in_trigger(
    sql: &str,
    table: Option<(&str, &str)>,
    columns: &[(String, String)],
    quote: &dyn Fn(&str) -> String,
) -> Option<String> {
    let lx = lex(sql);
    let name_at = lx.after_create("TRIGGER")?;
    let name_end = lx.skip_name(name_at);
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for (i, tok) in lx.toks.iter().enumerate() {
        if (name_at..name_end).contains(&i) || !matches!(tok.kind, Tok::Word | Tok::Quoted) {
            continue;
        }
        let value = lx.name(i);
        let table_new = table
            .filter(|(old, _)| old.eq_ignore_ascii_case(&value))
            .map(|(_, new)| new);
        let new = if lx.kind(i + 1) == Some(Tok::Dot) {
            table_new
        } else {
            columns
                .iter()
                .find(|(old, _)| old.eq_ignore_ascii_case(&value))
                .map(|(_, new)| new.as_str())
                .or(table_new)
        };
        if let Some(new) = new {
            edits.push((tok.start, tok.end, quote(new)));
        }
    }
    if edits.is_empty() {
        return None;
    }
    let mut out = sql.to_owned();
    for (start, end, text) in edits.into_iter().rev() {
        out.replace_range(start..end, &text);
    }
    Some(out)
}

/// A stored `CREATE TRIGGER` statement with its name qualified by `schema` (already quoted), so
/// it is re-created in that attached database; unchanged when the name is already qualified or
/// the text is not a `CREATE TRIGGER`.
pub(crate) fn qualify_trigger_name(sql: &str, schema: &str) -> String {
    let lx = lex(sql);
    match lx.after_create("TRIGGER") {
        Some(name_at) if lx.kind(name_at + 1) != Some(Tok::Dot) && name_at < lx.toks.len() => {
            let at = lx.toks[name_at].start;
            format!("{}{schema}.{}", &sql[..at], &sql[at..])
        }
        _ => sql.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquotes_every_identifier_form() {
        assert_eq!(unquote(r#""a""b""#), r#"a"b"#);
        assert_eq!(unquote("[a b]"), "a b");
        assert_eq!(unquote("`a``b`"), "a`b");
        assert_eq!(unquote("'it''s'"), "it's");
        assert_eq!(unquote("plain"), "plain");
        assert_eq!(unquote("\""), "\"");
        assert_eq!(unquote("\"\""), "");
        // One multi-byte character is longer than one byte but is not quoted.
        assert_eq!(unquote("é"), "é");
        assert_eq!(unquote("名"), "名");
        assert_eq!(unquote("π"), "π");
        assert_eq!(unquote("\"名\""), "名");
        assert_eq!(unquote("[é]"), "é");
        assert_eq!(unquote("名\""), "名\"");
    }

    #[test]
    fn reads_single_character_non_ascii_names() {
        let t = parse_create_table(
            "CREATE TABLE 表 (名 TEXT COLLATE é, é INT CONSTRAINT π CHECK (é > 0), \
             ü INT REFERENCES 表 (é), FOREIGN KEY (名) REFERENCES 表 (名))",
        );
        let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["名", "é", "ü"]);
        assert_eq!(t.columns[0].collation.as_deref(), Some("é"));
        assert_eq!(t.checks[0].name.as_deref(), Some("π"));
        assert_eq!(t.checks[0].column.as_deref(), Some("é"));
        assert_eq!(t.foreign_keys[0].ref_table, "表");
        assert_eq!(t.foreign_keys[0].ref_columns, ["é"]);
        assert_eq!(t.foreign_keys[1].columns, ["名"]);
        let trg = parse_create_trigger(
            "CREATE TRIGGER ť AFTER UPDATE OF 名, é ON 表 BEGIN SELECT 1; END",
        )
        .unwrap();
        assert_eq!(trg.update_columns, ["名", "é"]);
        let v = parse_create_view("CREATE VIEW ν (名) AS SELECT 1");
        assert_eq!(v.columns, ["名"]);
    }

    #[test]
    fn marks_column_level_primary_key_desc() {
        let t = parse_create_table(
            "CREATE TABLE t (a INTEGER PRIMARY KEY DESC, b INTEGER CONSTRAINT k PRIMARY KEY ASC)",
        );
        assert!(t.columns[0].pk_desc);
        assert!(!t.columns[1].pk_desc);
        let t = parse_create_table("CREATE TABLE t (a INTEGER, PRIMARY KEY (a DESC))");
        assert!(!t.columns[0].pk_desc);
    }

    #[test]
    fn lexes_comments_strings_and_numbers() {
        let lx = lex("a -- c ( \n /* ) */ 'x)' 1.5e-3 .5 \"q\"\"(\" [b)] X'00' ?");
        let kinds: Vec<Tok> = lx.toks.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            [
                Tok::Word,
                Tok::Str,
                Tok::Num,
                Tok::Num,
                Tok::Quoted,
                Tok::Quoted,
                Tok::Str,
                Tok::Other
            ]
        );
        assert_eq!(lx.text(2), "1.5e-3");
        // Unterminated input runs to the end.
        let lx = lex("'abc");
        assert_eq!(lx.text(0), "'abc");
        let lx = lex("x /* never closed");
        assert_eq!(lx.toks.len(), 1);
    }

    #[test]
    fn reads_the_fixture_tables() {
        let parent = parse_create_table(
            "CREATE TABLE m2_parent (\n  id INTEGER PRIMARY KEY,\n  code VARCHAR(20) NOT NULL \
             UNIQUE,\n  note TEXT NULL,\n  CONSTRAINT m2_parent_code_chk CHECK (length(code) > \
             0)\n)",
        );
        assert_eq!(
            parent
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["id", "code", "note"]
        );
        assert_eq!(
            parent.checks,
            [ParsedCheck {
                name: Some("m2_parent_code_chk".into()),
                expression: "length(code) > 0".into(),
                column: None,
            }]
        );
        assert_eq!(parent.foreign_keys, Vec::<ParsedForeignKey>::new());

        let child = parse_create_table(
            "CREATE TABLE m2_child (id INTEGER PRIMARY KEY, parent_id INTEGER NOT NULL, qty \
             INTEGER NOT NULL DEFAULT 1, CONSTRAINT m2_child_parent_fk FOREIGN KEY (parent_id) \
             REFERENCES m2_parent (id) ON DELETE CASCADE, CONSTRAINT m2_child_qty_chk CHECK (qty \
             >= 0))",
        );
        assert_eq!(
            child.foreign_keys,
            [ParsedForeignKey {
                name: Some("m2_child_parent_fk".into()),
                columns: vec!["parent_id".into()],
                ref_table: "m2_parent".into(),
                ref_columns: vec!["id".into()],
                deferrable: false,
                initially_deferred: false,
            }]
        );
        assert_eq!(child.checks[0].name.as_deref(), Some("m2_child_qty_chk"));
        assert_eq!(child.checks[0].expression, "qty >= 0");
    }

    #[test]
    fn reads_column_constraints() {
        let t = parse_create_table(
            r#"create temp table if not exists main."T x" (
                "id" integer constraint pk_t primary key autoincrement,
                [name] text collate nocase not null constraint name_chk check (name <> ''),
                ref int references other on delete set null on update no action
                    deferrable initially deferred,
                g int generated always as (id * 2) stored,
                h as (id + 1),
                d text default (lower('A,B')) check (d is not null), -- a comment, with a comma
                `e` blob
            ) without rowid, strict"#,
        );
        let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "name", "ref", "g", "h", "d", "e"]);
        assert_eq!(t.pk_name.as_deref(), Some("pk_t"));
        assert!(t.columns[0].autoincrement);
        assert_eq!(t.columns[1].collation.as_deref(), Some("nocase"));
        assert_eq!(t.columns[3].generated.as_deref(), Some("id * 2"));
        assert_eq!(t.columns[4].generated.as_deref(), Some("id + 1"));
        assert!(t.without_rowid && t.strict);
        assert_eq!(
            t.checks,
            [
                ParsedCheck {
                    name: Some("name_chk".into()),
                    expression: "name <> ''".into(),
                    column: Some("name".into()),
                },
                ParsedCheck {
                    name: None,
                    expression: "d is not null".into(),
                    column: Some("d".into()),
                },
            ]
        );
        assert_eq!(
            t.foreign_keys,
            [ParsedForeignKey {
                name: None,
                columns: vec!["ref".into()],
                ref_table: "other".into(),
                ref_columns: vec![],
                deferrable: true,
                initially_deferred: true,
            }]
        );
    }

    #[test]
    fn reads_table_constraints_without_commas_and_virtual_tables() {
        let t = parse_create_table(
            "CREATE TABLE t(a, b, CONSTRAINT c1 CHECK(a > 0) CONSTRAINT c2 CHECK(b > 0) \
             FOREIGN KEY (a, b) REFERENCES p(x, y), PRIMARY KEY (a))",
        );
        assert_eq!(
            t.checks
                .iter()
                .map(|c| c.name.as_deref())
                .collect::<Vec<_>>(),
            [Some("c1"), Some("c2")]
        );
        assert_eq!(t.foreign_keys[0].columns, ["a", "b"]);
        assert_eq!(t.foreign_keys[0].ref_columns, ["x", "y"]);
        assert_eq!(t.foreign_keys[0].name, None);
        assert_eq!(t.pk_name, None);
        assert!(!t.without_rowid && !t.strict);

        let v = parse_create_table("CREATE VIRTUAL TABLE docs USING fts5(title, body)");
        assert_eq!(v.module.as_deref(), Some("fts5"));
        assert_eq!(v.columns, Vec::<ParsedColumn>::new());

        let ctas = parse_create_table("CREATE TABLE copy AS SELECT 1 AS x");
        assert_eq!(ctas, ParsedTable::default());
        assert_eq!(
            parse_create_table("not sql at all ("),
            ParsedTable::default()
        );
    }

    #[test]
    fn reads_indexes() {
        let i = parse_create_index(
            "CREATE UNIQUE INDEX IF NOT EXISTS ix ON t (a COLLATE nocase DESC, lower(b), c ASC) \
             WHERE a IS NOT NULL",
        );
        assert_eq!(i.parts, ["a", "lower(b)", "c"]);
        assert_eq!(i.predicate.as_deref(), Some("a IS NOT NULL"));
        let plain = parse_create_index("CREATE INDEX m2_child_parent_idx ON m2_child (parent_id)");
        assert_eq!(plain.parts, ["parent_id"]);
        assert_eq!(plain.predicate, None);
    }

    #[test]
    fn reads_views() {
        let v = parse_create_view(
            "CREATE VIEW m2_view AS SELECT c.id, c.qty, p.code FROM m2_child c JOIN m2_parent p \
             ON p.id = c.parent_id",
        );
        assert_eq!(v.columns, Vec::<String>::new());
        assert_eq!(
            v.definition,
            "SELECT c.id, c.qty, p.code FROM m2_child c JOIN m2_parent p ON p.id = c.parent_id"
        );
        let v = parse_create_view("create temp view if not exists s.v(a, \"b\") as\n select 1, 2;");
        assert_eq!(v.columns, ["a", "b"]);
        assert_eq!(v.definition, "select 1, 2");
    }

    #[test]
    fn reads_triggers() {
        let t = parse_create_trigger(
            "CREATE TRIGGER m2_trg BEFORE INSERT ON m2_child FOR EACH ROW WHEN (NEW.qty < 0) \
             BEGIN SELECT RAISE(ABORT, 'no'); END",
        )
        .unwrap();
        assert_eq!(t.timing, ParsedTiming::Before);
        assert_eq!(t.event, ParsedEvent::Insert);
        assert_eq!(t.condition.as_deref(), Some("NEW.qty < 0"));
        assert_eq!(t.body, "BEGIN SELECT RAISE(ABORT, 'no'); END");

        let t = parse_create_trigger(
            "create trigger if not exists main.tu update of a, \"b\" on t when new.a > 1 and \
             (new.b) begin select 1; end",
        )
        .unwrap();
        assert_eq!(t.timing, ParsedTiming::Before);
        assert_eq!(t.event, ParsedEvent::Update);
        assert_eq!(t.update_columns, ["a", "b"]);
        assert_eq!(t.condition.as_deref(), Some("new.a > 1 and (new.b)"));

        let t = parse_create_trigger(
            "CREATE TRIGGER vi INSTEAD OF DELETE ON v BEGIN DELETE FROM t WHERE id = OLD.id; END",
        )
        .unwrap();
        assert_eq!(t.timing, ParsedTiming::InsteadOf);
        assert_eq!(t.event, ParsedEvent::Delete);
        assert_eq!(t.condition, None);
        assert_eq!(t.table_schema, None);
        assert!(parse_create_trigger("CREATE TABLE x(a)").is_none());

        // `begin` as a column name does not open the body.
        let t = parse_create_trigger(
            "CREATE TEMP TRIGGER tb AFTER INSERT ON main.x WHEN NEW.begin > 0 AND (begin) \
             BEGIN SELECT NEW.begin; END",
        )
        .unwrap();
        assert_eq!(t.table_schema.as_deref(), Some("main"));
        assert_eq!(t.condition.as_deref(), Some("NEW.begin > 0 AND (begin)"));
        assert_eq!(t.body, "BEGIN SELECT NEW.begin; END");
    }

    #[test]
    fn m3_identifier_helpers() {
        assert!(mentions_identifier("qty >= 0 AND \"Note\" <> ''", "note"));
        assert!(mentions_identifier("[qty] > 1", "QTY"));
        assert!(!mentions_identifier("'qty' = x", "qty"));
        assert!(!mentions_identifier("quantity > 0", "qty"));

        let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let sql = "CREATE TRIGGER qty AFTER UPDATE OF qty ON t BEGIN \
                   UPDATE t SET note = 'qty' WHERE t.qty = NEW.qty; END";
        let out = rename_in_trigger(
            sql,
            Some(("t", "u")),
            &[("qty".to_owned(), "amount".to_owned())],
            &quote,
        )
        .unwrap();
        assert_eq!(
            out,
            "CREATE TRIGGER qty AFTER UPDATE OF \"amount\" ON \"u\" BEGIN \
             UPDATE \"u\" SET note = 'qty' WHERE \"u\".\"amount\" = NEW.\"amount\"; END"
        );
        assert_eq!(rename_in_trigger(sql, None, &[], &quote), None);
        assert_eq!(
            rename_in_trigger("CREATE TABLE t (a)", Some(("t", "u")), &[], &quote),
            None
        );

        assert_eq!(
            qualify_trigger_name(
                "CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END",
                "\"aux\""
            ),
            "CREATE TRIGGER \"aux\".tr AFTER INSERT ON t BEGIN SELECT 1; END"
        );
        let qualified = "CREATE TRIGGER IF NOT EXISTS x.tr AFTER INSERT ON t BEGIN SELECT 1; END";
        assert_eq!(qualify_trigger_name(qualified, "\"aux\""), qualified);
        assert_eq!(qualify_trigger_name("SELECT 1", "\"aux\""), "SELECT 1");
    }
}
