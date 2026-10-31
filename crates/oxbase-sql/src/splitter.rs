//! [`crate::split`]: cuts a script into statements with one forward pass over the tokens of
//! [`crate::lex`]. See the crate docs for the rules.
//!
//! Besides the lexer, the splitter keeps three small pieces of state:
//!
//! - the pending statement (start of its first content token, end of its last content or
//!   comment token);
//! - the MySQL client delimiter (`DELIMITER` lines, recognised only at the start of a line
//!   while no statement is pending);
//! - [`Blocks`], which counts the compound bodies in which `;` does not end the statement
//!   (PostgreSQL `BEGIN ATOMIC`, MySQL/MariaDB stored programs, SQLite trigger bodies).
//!
//! Positions are computed incrementally (statement offsets only grow), so the whole split is
//! linear in the length of the script.
//!
//! Beyond the crate docs (best effort, MariaDB only): the top-level compound statements
//! `IF`, `CASE`, `LOOP`, `WHILE`, `REPEAT` and `FOR` that MariaDB accepts outside stored
//! programs are also kept whole, and `/*M! ... */` is executable content like `/*! ... */`.

use crate::lex::{self, Rules, Token, TokenKind};
use crate::{Position, SqlDialect, Statement, StatementKind};

pub(crate) fn split(dialect: SqlDialect, text: &str) -> Vec<Statement> {
    let mut splitter = Splitter::new(dialect, text);
    splitter.run();
    splitter.out
}

/// First keywords of statements that probably return rows (see [`StatementKind::Query`]).
const QUERY_WORDS: [&[u8]; 11] = [
    b"SELECT",
    b"WITH",
    b"VALUES",
    b"TABLE",
    b"SHOW",
    b"DESCRIBE",
    b"DESC",
    b"EXPLAIN",
    b"PRAGMA",
    b"CALL",
    b"FETCH",
];

struct Splitter<'a> {
    rules: Rules,
    text: &'a str,
    out: Vec<Statement>,
    positions: Positions<'a>,
    /// The statement terminator: `;`, or the MySQL client delimiter.
    delimiter: Vec<u8>,
    /// Ends words early: the MySQL delimiter when it starts with a word byte, else empty.
    word_stop: Vec<u8>,
    /// Start of the pending statement's first content token.
    start: Option<usize>,
    /// End of the pending statement's last content or comment token.
    end: usize,
    kind: StatementKind,
    blocks: Blocks,
    /// Only spaces and tabs since the last line feed (or the start of the text).
    at_line_start: bool,
}

impl<'a> Splitter<'a> {
    fn new(dialect: SqlDialect, text: &'a str) -> Self {
        Self {
            rules: Rules::new(dialect),
            text,
            out: Vec::new(),
            positions: Positions::new(text),
            delimiter: b";".to_vec(),
            word_stop: Vec::new(),
            start: None,
            end: 0,
            kind: StatementKind::Other,
            blocks: Blocks::new(dialect, true),
            at_line_start: true,
        }
    }

    fn run(&mut self) {
        let bytes = self.text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if let Some(next) = self.directive_or_batch_end(i) {
                i = next;
                continue;
            }
            if let Some(next) = self.terminator(i) {
                i = next;
                continue;
            }
            let tok = lex::token_at(self.text, i, self.rules, &self.word_stop);
            self.on_token(tok);
            i = tok.end;
        }
        let end = self.end;
        self.finish(end);
    }

    /// A MySQL `DELIMITER` line or a T-SQL `GO` line at `i`: handles it and returns the end of
    /// its line (before the line feed).
    fn directive_or_batch_end(&mut self, i: usize) -> Option<usize> {
        if !self.at_line_start {
            return None;
        }
        let bytes = self.text.as_bytes();
        match self.rules.dialect {
            SqlDialect::MySql { .. } if self.start.is_none() => {
                let (line_end, delimiter) = delimiter_directive(bytes, i)?;
                if !delimiter.is_empty() {
                    self.set_delimiter(delimiter.to_vec());
                }
                self.finish(line_end);
                self.at_line_start = false;
                Some(line_end)
            }
            SqlDialect::TSql => {
                let line_end = go_line(bytes, i)?;
                self.finish(line_end);
                self.at_line_start = false;
                Some(line_end)
            }
            _ => None,
        }
    }

    fn set_delimiter(&mut self, delimiter: Vec<u8>) {
        let default = delimiter == b";";
        let starts_with_word_byte = delimiter
            .first()
            .is_some_and(|&b| self.rules.is_word_byte(b));
        self.word_stop = if !default && starts_with_word_byte {
            delimiter.clone()
        } else {
            Vec::new()
        };
        self.delimiter = delimiter;
        // Compound bodies are only tracked with the default delimiter: a script that changes
        // it delimits its compound statements itself.
        self.blocks = Blocks::new(self.rules.dialect, default);
    }

    /// The statement terminator at `i` outside any compound body: ends the pending statement
    /// and returns the byte after the terminator.
    fn terminator(&mut self, i: usize) -> Option<usize> {
        if self.rules.dialect == SqlDialect::TSql
            || !self.text.as_bytes()[i..].starts_with(&self.delimiter)
            || self.blocks.depth() > 0
        {
            return None;
        }
        let next = i + self.delimiter.len();
        self.finish(next);
        self.at_line_start = false;
        Some(next)
    }

    fn on_token(&mut self, tok: Token) {
        let bytes = self.text.as_bytes();
        match tok.kind {
            TokenKind::Space => {
                for &b in &bytes[tok.start..tok.end] {
                    match b {
                        b'\n' => self.at_line_start = true,
                        b' ' | b'\t' => {}
                        _ => self.at_line_start = false,
                    }
                }
            }
            TokenKind::LineComment | TokenKind::BlockComment => {
                self.at_line_start = false;
                // Leading comments are not part of the statement; later ones are.
                if self.start.is_some() {
                    self.end = tok.end;
                }
            }
            _ => {
                self.at_line_start = false;
                if self.start.is_none() {
                    self.start = Some(tok.start);
                    self.kind = statement_kind(bytes, tok);
                }
                self.end = tok.end;
                if self.blocks.active() {
                    let signal = match tok.kind {
                        TokenKind::Word => Signal::Word(Kw::of(&bytes[tok.start..tok.end])),
                        TokenKind::Punct if tok.end == tok.start + 1 => {
                            Signal::Punct(bytes[tok.start])
                        }
                        _ => Signal::Other,
                    };
                    self.blocks.feed(signal);
                }
            }
        }
    }

    /// Ends the pending segment at `segment_end`: emits its statement, if it has content.
    fn finish(&mut self, segment_end: usize) {
        if let Some(start) = self.start.take() {
            let end = self.end;
            let start_pos = self.positions.at(start);
            let end_pos = self.positions.at(end);
            self.out.push(Statement {
                range: start..end,
                segment_end: segment_end.max(end),
                start: start_pos,
                end: end_pos,
                text: self
                    .text
                    .get(start..end)
                    .map_or_else(String::new, str::to_owned),
                kind: self.kind,
            });
        }
        self.blocks.reset();
    }
}

/// [`StatementKind`] from the first content token.
fn statement_kind(bytes: &[u8], tok: Token) -> StatementKind {
    let query = match tok.kind {
        TokenKind::Word => {
            let word = &bytes[tok.start..tok.end];
            QUERY_WORDS.iter().any(|k| word.eq_ignore_ascii_case(k))
        }
        TokenKind::Punct => bytes[tok.start] == b'(',
        _ => false,
    };
    if query {
        StatementKind::Query
    } else {
        StatementKind::Other
    }
}

/// `DELIMITER <token>` at `i` (case-insensitive keyword, then spaces or tabs, then the new
/// delimiter: the run of non-whitespace bytes). Returns the end of the line (before its line
/// feed) and the delimiter (empty when the line has none: the line is still a directive).
fn delimiter_directive(bytes: &[u8], i: usize) -> Option<(usize, &[u8])> {
    const KEYWORD: &[u8] = b"DELIMITER";
    if !bytes
        .get(i..i + KEYWORD.len())?
        .eq_ignore_ascii_case(KEYWORD)
    {
        return None;
    }
    let mut j = i + KEYWORD.len();
    if bytes.get(j).is_some_and(|&c| !lex::is_space(c)) {
        return None;
    }
    while matches!(bytes.get(j), Some(b' ' | b'\t')) {
        j += 1;
    }
    let delimiter_start = j;
    while bytes.get(j).is_some_and(|&c| !lex::is_space(c)) {
        j += 1;
    }
    let line_end = line_end(bytes, j);
    Some((line_end, &bytes[delimiter_start..j]))
}

/// A T-SQL batch separator at `i`: a line holding only `GO` (case-insensitive), an optional
/// count and an optional `--` comment. Returns the end of the line (before its line feed).
fn go_line(bytes: &[u8], i: usize) -> Option<usize> {
    if !bytes.get(i..i + 2)?.eq_ignore_ascii_case(b"GO") {
        return None;
    }
    let tsql = Rules::new(SqlDialect::TSql);
    let mut j = i + 2;
    if bytes.get(j).is_some_and(|&c| tsql.is_word_byte(c)) {
        return None;
    }
    let skip_blanks = |mut j: usize| {
        while bytes
            .get(j)
            .is_some_and(|&c| c != b'\n' && lex::is_space(c))
        {
            j += 1;
        }
        j
    };
    j = skip_blanks(j);
    while bytes.get(j).is_some_and(u8::is_ascii_digit) {
        j += 1;
    }
    j = skip_blanks(j);
    if bytes[j..].starts_with(b"--") {
        j = line_end(bytes, j);
    }
    match bytes.get(j) {
        None | Some(b'\n') => Some(j),
        Some(_) => None,
    }
}

/// The position of the next line feed at or after `i`, or the text end.
fn line_end(bytes: &[u8], i: usize) -> usize {
    bytes[i..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(bytes.len(), |p| i + p)
}

/// Incremental [`crate::position_at`] for growing offsets.
struct Positions<'a> {
    text: &'a str,
    offset: usize,
    line: u32,
    column: u32,
}

impl<'a> Positions<'a> {
    const fn new(text: &'a str) -> Self {
        Self {
            text,
            offset: 0,
            line: 1,
            column: 1,
        }
    }

    fn at(&mut self, offset: usize) -> Position {
        let Some(bytes) = self.text.as_bytes().get(self.offset..offset) else {
            // Not reached (offsets only grow); stay correct anyway.
            return crate::position_at(self.text, offset);
        };
        for &b in bytes {
            if b == b'\n' {
                self.line = self.line.saturating_add(1);
                self.column = 1;
            } else if b & 0xC0 != 0x80 {
                // A character starts here: 4-byte characters are two UTF-16 units.
                self.column = self.column.saturating_add(if b >= 0xF0 { 2 } else { 1 });
            }
        }
        self.offset = offset;
        Position {
            line: self.line,
            column: self.column,
        }
    }
}

/// The keywords [`Blocks`] looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kw {
    Aggregate,
    Atomic,
    Begin,
    Case,
    Create,
    Definer,
    Do,
    Else,
    End,
    Event,
    For,
    Function,
    If,
    Loop,
    Not,
    Or,
    Procedure,
    Repeat,
    Replace,
    Row,
    Temp,
    Then,
    Trigger,
    While,
    Other,
}

impl Kw {
    fn of(word: &[u8]) -> Self {
        let mut buf = [0_u8; 9];
        let Some(upper) = buf.get_mut(..word.len()) else {
            return Self::Other;
        };
        for (dst, src) in upper.iter_mut().zip(word) {
            *dst = src.to_ascii_uppercase();
        }
        match &*upper {
            b"AGGREGATE" => Self::Aggregate,
            b"ATOMIC" => Self::Atomic,
            b"BEGIN" => Self::Begin,
            b"CASE" => Self::Case,
            b"CREATE" => Self::Create,
            b"DEFINER" => Self::Definer,
            b"DO" => Self::Do,
            b"ELSE" => Self::Else,
            b"END" => Self::End,
            b"EVENT" => Self::Event,
            b"FOR" => Self::For,
            b"FUNCTION" => Self::Function,
            b"IF" => Self::If,
            b"LOOP" => Self::Loop,
            b"NOT" => Self::Not,
            b"OR" => Self::Or,
            b"PROCEDURE" => Self::Procedure,
            b"REPEAT" => Self::Repeat,
            b"REPLACE" => Self::Replace,
            b"ROW" => Self::Row,
            b"TEMP" | b"TEMPORARY" => Self::Temp,
            b"THEN" => Self::Then,
            b"TRIGGER" => Self::Trigger,
            b"WHILE" => Self::While,
            _ => Self::Other,
        }
    }
}

/// A content token as [`Blocks`] sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    Word(Kw),
    /// A one-byte punctuation token.
    Punct(u8),
    /// Strings, quoted identifiers, executable comments, `:=`.
    Other,
}

/// An open block of a compound body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    /// `BEGIN` (`BEGIN ATOMIC`, `BEGIN NOT ATOMIC`, a trigger body).
    Begin,
    /// MySQL `IF ... END IF`.
    If,
    /// MySQL `CASE ... END CASE` statement.
    CaseStatement,
    /// `CASE ... END` expression.
    CaseExpression,
    /// MySQL `LOOP`, `WHILE`, `REPEAT`, MariaDB `FOR` (closed by `END <keyword>`).
    Loop,
}

/// How far the start of the pending statement has been recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Header {
    /// No content token yet.
    Start,
    /// `CREATE` seen, waiting for the object keyword.
    Create,
    /// MariaDB `BEGIN` seen: `NOT ATOMIC` makes it a compound statement.
    Begin,
    /// MariaDB `BEGIN NOT` seen.
    BeginNot,
    /// A statement whose compound body is tracked.
    Compound,
    /// Anything else: `;` ends it (PostgreSQL tracks `BEGIN ATOMIC` without a header).
    Plain,
}

/// Compound-body tracking for one statement. `depth() > 0` means a `;` is inside a body.
#[derive(Debug)]
struct Blocks {
    dialect: SqlDialect,
    /// Off for T-SQL and while a custom MySQL delimiter is in force.
    enabled: bool,
    stack: Vec<Block>,
    header: Header,
    /// Tokens seen after `CREATE` while looking for the object keyword.
    header_tokens: u32,
    /// `DEFINER = ...` seen in a MySQL `CREATE` header (the user name may be any tokens).
    definer: bool,
    /// The previous content token's keyword (`Other` for non-keywords and non-words).
    prev: Kw,
    /// MySQL: the next word starts a statement of the body (block keywords count there).
    statement_start: bool,
    /// MySQL: the previous word was `END` (so `IF`, `CASE`, `LOOP`, ... now close, not open).
    after_end: bool,
    /// SQLite: the trigger body has been closed.
    body_done: bool,
}

impl Blocks {
    fn new(dialect: SqlDialect, default_delimiter: bool) -> Self {
        Self {
            dialect,
            enabled: default_delimiter && dialect != SqlDialect::TSql,
            stack: Vec::new(),
            header: Header::Start,
            header_tokens: 0,
            definer: false,
            prev: Kw::Other,
            statement_start: false,
            after_end: false,
            body_done: false,
        }
    }

    const fn active(&self) -> bool {
        self.enabled
    }

    fn depth(&self) -> usize {
        self.stack.len()
    }

    fn reset(&mut self) {
        self.stack.clear();
        self.header = Header::Start;
        self.header_tokens = 0;
        self.definer = false;
        self.prev = Kw::Other;
        self.statement_start = false;
        self.after_end = false;
        self.body_done = false;
    }

    fn feed(&mut self, signal: Signal) {
        match self.dialect {
            SqlDialect::Postgres => self.feed_postgres(signal),
            SqlDialect::Sqlite => self.feed_sqlite(signal),
            SqlDialect::MySql { mariadb } => self.feed_mysql(signal, mariadb),
            SqlDialect::TSql => {}
        }
        self.prev = match signal {
            Signal::Word(kw) => kw,
            _ => Kw::Other,
        };
    }

    /// `BEGIN ATOMIC ... END`, with `CASE ... END` counted inside.
    fn feed_postgres(&mut self, signal: Signal) {
        match signal {
            Signal::Word(Kw::Atomic) if self.prev == Kw::Begin => self.stack.push(Block::Begin),
            Signal::Word(Kw::Case) if !self.stack.is_empty() => {
                self.stack.push(Block::CaseExpression);
            }
            Signal::Word(Kw::End) => {
                self.stack.pop();
            }
            _ => {}
        }
    }

    /// `CREATE [TEMP|TEMPORARY] TRIGGER ... BEGIN ... END`, with `CASE ... END` inside.
    fn feed_sqlite(&mut self, signal: Signal) {
        let Signal::Word(kw) = signal else {
            if self.header != Header::Compound {
                self.header = Header::Plain;
            }
            return;
        };
        match (self.header, kw) {
            (Header::Start, Kw::Create) => self.header = Header::Create,
            (Header::Create, Kw::Temp) => {}
            (Header::Create, Kw::Trigger) => self.header = Header::Compound,
            (Header::Compound, _) => match kw {
                Kw::Begin if self.stack.is_empty() && !self.body_done => {
                    self.stack.push(Block::Begin);
                }
                Kw::Case if !self.stack.is_empty() => self.stack.push(Block::CaseExpression),
                Kw::End if !self.stack.is_empty() => {
                    self.stack.pop();
                    self.body_done = self.stack.is_empty();
                }
                _ => {}
            },
            _ => self.header = Header::Plain,
        }
    }

    fn feed_mysql(&mut self, signal: Signal, mariadb: bool) {
        match self.header {
            Header::Compound => self.feed_mysql_body(signal),
            Header::Plain => {}
            Header::Start => match signal {
                Signal::Word(Kw::Create) => self.header = Header::Create,
                Signal::Word(Kw::Begin) if mariadb => self.header = Header::Begin,
                Signal::Word(Kw::If | Kw::Case | Kw::Loop | Kw::While | Kw::Repeat | Kw::For)
                    if mariadb =>
                {
                    self.header = Header::Compound;
                    self.statement_start = true;
                    self.feed_mysql_body(signal);
                }
                _ => self.header = Header::Plain,
            },
            Header::Begin => {
                self.header = if signal == Signal::Word(Kw::Not) {
                    Header::BeginNot
                } else {
                    Header::Plain
                };
            }
            Header::BeginNot => {
                if signal == Signal::Word(Kw::Atomic) {
                    self.header = Header::Compound;
                    self.stack.push(Block::Begin);
                    self.statement_start = true;
                } else {
                    self.header = Header::Plain;
                }
            }
            Header::Create => {
                self.header_tokens += 1;
                match signal {
                    Signal::Word(Kw::Procedure | Kw::Function | Kw::Trigger | Kw::Event) => {
                        self.header = Header::Compound;
                    }
                    Signal::Word(Kw::Or | Kw::Replace | Kw::Aggregate) => {}
                    Signal::Word(Kw::Definer) => self.definer = true,
                    // `DEFINER = 'user'@'host'`, `CURRENT_USER()`, `` `u`@`%` ``, ...
                    _ if self.definer && self.header_tokens < 16 => {}
                    _ => self.header = Header::Plain,
                }
            }
        }
    }

    /// Body of a MySQL/MariaDB compound statement. `BEGIN` always opens a block; `IF`, `LOOP`,
    /// `WHILE`, `REPEAT`, `FOR` and statement `CASE` open one only where a statement starts
    /// (after `;`, `BEGIN`, `THEN`, `ELSE`, `DO`, `LOOP`, `REPEAT`, a label `:` or a trigger's
    /// `FOR EACH ROW`), so `IF(...)`, `IF EXISTS` and `REPEAT(...)` do not; `CASE` elsewhere is
    /// an expression. `END` closes the innermost block and consumes a following block keyword
    /// (`END IF`, `END LOOP`, ...).
    fn feed_mysql_body(&mut self, signal: Signal) {
        let kw = match signal {
            Signal::Word(kw) => kw,
            Signal::Punct(b';' | b':') => {
                self.statement_start = true;
                self.after_end = false;
                return;
            }
            Signal::Punct(_) | Signal::Other => {
                self.statement_start = false;
                self.after_end = false;
                return;
            }
        };
        let after_end = std::mem::replace(&mut self.after_end, false);
        let statement_start = std::mem::replace(&mut self.statement_start, false);
        if after_end
            && matches!(
                kw,
                Kw::If | Kw::Case | Kw::Loop | Kw::While | Kw::Repeat | Kw::For
            )
        {
            return;
        }
        match kw {
            Kw::Begin => {
                self.stack.push(Block::Begin);
                self.statement_start = true;
            }
            // `BEGIN NOT ATOMIC`: the body starts after `ATOMIC`.
            Kw::Not | Kw::Atomic if matches!(self.prev, Kw::Begin | Kw::Not) => {
                self.statement_start = statement_start;
            }
            Kw::If if statement_start => self.stack.push(Block::If),
            Kw::Case => self.stack.push(if statement_start {
                Block::CaseStatement
            } else {
                Block::CaseExpression
            }),
            Kw::Loop | Kw::Repeat if statement_start => {
                self.stack.push(Block::Loop);
                self.statement_start = true;
            }
            Kw::While | Kw::For if statement_start => self.stack.push(Block::Loop),
            Kw::End => {
                self.stack.pop();
                self.after_end = true;
            }
            Kw::Then | Kw::Else => {
                self.statement_start = self.stack.last() != Some(&Block::CaseExpression);
            }
            Kw::Do => self.statement_start = true,
            Kw::Row if self.stack.is_empty() => self.statement_start = true,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_are_case_insensitive() {
        assert_eq!(Kw::of(b"begin"), Kw::Begin);
        assert_eq!(Kw::of(b"TeMpOrArY"), Kw::Temp);
        assert_eq!(Kw::of(b"PROCEDURES"), Kw::Other);
        assert_eq!(Kw::of(b""), Kw::Other);
        assert_eq!(Kw::of("éé".as_bytes()), Kw::Other);
    }

    #[test]
    fn delimiter_directives() {
        assert_eq!(
            delimiter_directive(b"DELIMITER $$\nX", 0),
            Some((12, &b"$$"[..]))
        );
        assert_eq!(
            delimiter_directive(b"delimiter\t// trailing", 0),
            Some((21, &b"//"[..]))
        );
        assert_eq!(delimiter_directive(b"DELIMITER", 0), Some((9, &b""[..])));
        assert_eq!(delimiter_directive(b"DELIMITERS ;", 0), None);
        assert_eq!(delimiter_directive(b"DELIM", 0), None);
    }

    #[test]
    fn go_lines() {
        assert_eq!(go_line(b"GO", 0), Some(2));
        assert_eq!(go_line(b"go 12 -- x\nSELECT", 0), Some(10));
        assert_eq!(go_line(b"GO\r\n", 0), Some(3));
        assert_eq!(go_line(b"GOTO x", 0), None);
        assert_eq!(go_line(b"GO x", 0), None);
        assert_eq!(go_line(b"G", 0), None);
    }

    #[test]
    fn incremental_positions_match_position_at() {
        let text = "a\r\nxé😀\n\nz";
        let mut positions = Positions::new(text);
        for offset in (0..=text.len()).filter(|&o| text.is_char_boundary(o)) {
            assert_eq!(
                positions.at(offset),
                crate::position_at(text, offset),
                "{offset}"
            );
        }
    }
}
