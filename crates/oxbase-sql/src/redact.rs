//! [`crate::redact_secrets`]: masks password literals before SQL text is stored in history.
//!
//! One pass over the tokens of [`crate::lex`] (comments are skipped, so keywords inside them
//! are ignored; MySQL executable comments are opened so the keywords inside them count). A
//! small state machine arms on a secret keyword and replaces the next string literal. Every
//! literal that is not replaced is scanned for `password=` / `pwd=` connection-string values
//! (rule 2) and is then redacted again as SQL text, so statements nested in literals are
//! masked too: dynamic SQL (`EXECUTE 'ALTER ROLE r PASSWORD ''x'''`) and PostgreSQL
//! dollar-quoted bodies (`DO $$ ... $$`, `CREATE FUNCTION ... AS $$ ... $$`). Nesting is
//! followed [`MAX_DEPTH`] levels deep, so the whole pass stays linear.
//!
//! Beyond the crate docs, also masked (best effort): MySQL `SET PASSWORD FOR user = '...'`,
//! `OLD_PASSWORD('...')`, the old password of `ALTER USER ... IDENTIFIED BY '...' REPLACE
//! '...'`, MariaDB `IDENTIFIED VIA plugin USING '...'`, a backslash-quoted MySQL
//! connection-string value (`password=\'a b\'`), PostgreSQL `U&'...'` literals, and SQLite /
//! SQLCipher `PRAGMA key|rekey|hexkey|hexrekey = ...` (string, double-quoted or bare-word
//! value), `ATTACH ... KEY '...'` and `key=` / `hexkey=` values inside literals (URIs).
//!
//! T-SQL (`docs/m4-contracts.md` §6): `PASSWORD =` and `SECRET =` cover `CREATE` / `ALTER
//! LOGIN`, `CREATE USER ... WITH PASSWORD`, `ENCRYPTION` / `DECRYPTION BY PASSWORD`,
//! credentials (`IDENTITY = '...', SECRET = '...'`) and application roles; `OLD_PASSWORD =`
//! and `NEW_PASSWORD =` (`ALTER SERVICE MASTER KEY`) too. An `N'...'` secret keeps its `N`
//! (`N'***'`); a `"..."` secret (a string under `SET QUOTED_IDENTIFIER OFF`) is masked too.
//! Also masked (best effort):
//!
//! - a hashed password `PASSWORD = 0x... HASHED`, `MEDIAPASSWORD = '...'`, `KEY_SOURCE =
//!   '...'`, the first argument of `PWDENCRYPT(...)` / `PWDCOMPARE(...)`;
//! - the value assigned to a variable or named argument whose name holds `password`,
//!   `passwd`, `pwd` or `secret`, or is `@pw`, `@pass`, `@new` or `@old` (`sp_password`):
//!   `@rmtpassword = N'...'`, `SET @NewPwd = '...'`, `DECLARE @password nvarchar(128) =
//!   N'...'` (a type name and its `(n)` / `(max)` may sit between the name and `=`);
//! - the password position of system procedures called with positional arguments:
//!   `sp_addlogin` (2nd), `sp_password` (1st and 2nd), `sp_setapprole`, `sp_addapprole`,
//!   `sp_approlepassword`, `sp_control_dbmasterkey_password` (2nd), `sp_change_users_login`
//!   (4th) and `sp_addlinkedsrvlogin` (5th).
//!
//! Dynamic SQL in `EXEC ('...')` / `sp_executesql N'...'` literals is redacted like any nested
//! SQL. Not masked: the values of `sp_executesql` parameters (`@p = N'...'` with a name that
//! does not look like a secret), since nothing in the text says which one is a secret.
//!
//! Deliberate narrowings:
//!
//! - `KEY` (and `REKEY`, `HEXKEY`, `HEXREKEY`) arms only in the SQLite dialect and only in a
//!   `PRAGMA` or `ATTACH` statement, so key-value queries (`WHERE key = 'user:42'`) keep their
//!   literals in history.
//! - Rule 2 is not applied to the raw text of a dollar-quoted body that looks like code (it
//!   holds a `;` or a line feed), so `IF NEW.password = OLD.password` in a function body is
//!   kept; the literals inside that body still get rule 2.

use std::borrow::Cow;

use crate::lex::{self, Rules, TokenKind};
use crate::{REDACTED_LITERAL, SqlDialect};

/// What replaces a connection-string password value.
const REDACTED_VALUE: &str = "***";

/// How many literal levels deep nested SQL is redacted (a `DO $$ ... $$` body is one level, a
/// dynamic SQL string inside it is two). Each level re-lexes at most the text of the level
/// above, so the work stays linear in the input.
const MAX_DEPTH: u8 = 3;

/// Connection-string keys of rule 2, lowercase, every dialect.
const VALUE_KEYS: &[&[u8]] = &[b"password", b"pwd"];
/// SQLite also masks SQLCipher URI parameters (`file:x.db?key=...`).
const SQLITE_VALUE_KEYS: &[&[u8]] = &[b"password", b"pwd", b"hexkey", b"key"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Off,
    /// The next string literal is a secret. `paren`: a `(` was skipped since arming;
    /// `password`: armed by `PASSWORD` itself (so `FOR user` may follow).
    Armed {
        paren: bool,
        password: bool,
    },
    /// After `WITH` / `VIA`: the next word or literal is an authentication plugin name.
    Plugin {
        paren: bool,
    },
    /// MySQL `SET PASSWORD FOR user`: skip the user name up to `=`.
    ForUser,
    /// A secret was just masked (`REPLACE '<old>'` may follow).
    AfterSecret,
    /// T-SQL: after a secret variable or argument (`@password`); `=` arms. `typed`: a type
    /// name followed (`DECLARE @pwd nvarchar`); `open`: inside its `(n)` / `(max)`.
    VarDecl {
        typed: bool,
        open: bool,
    },
    /// T-SQL: the positional arguments of a system procedure; bit `arg` of `secrets` set: the
    /// current argument is a password.
    Positional {
        secrets: u8,
        arg: u8,
    },
}

/// The first word of the current statement, as far as `KEY` arming cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Head {
    /// No content token yet.
    Start,
    /// `PRAGMA` or `ATTACH`: SQLCipher keys may follow.
    Keyed,
    Other,
}

/// Words the state machine reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Word {
    /// Arms; `password` is true for `PASSWORD` itself.
    Secret {
        password: bool,
    },
    /// `KEY`, `REKEY`, `HEXKEY`, `HEXREKEY`: arm in a SQLite `PRAGMA` / `ATTACH` only.
    Key,
    /// `BY`, `AS`, `USING`: may sit between the keyword and the literal.
    Filler,
    /// `WITH`, `VIA`: a plugin name follows.
    Plugin,
    For,
    Replace,
    /// T-SQL: a variable or argument whose name looks like a secret (`@password`, `@pwd`).
    SecretVar,
    /// T-SQL: a system procedure with a password among its positional arguments (bit `i`:
    /// argument `i`, from 0).
    Proc(u8),
    Other,
}

impl Word {
    fn of(dialect: SqlDialect, word: &[u8]) -> Self {
        let tsql = dialect == SqlDialect::TSql;
        if tsql {
            // `@name`: a variable or argument, never a keyword (`@@name`: a function).
            if let [b'@', name @ ..] = word
                && name.first() != Some(&b'@')
            {
                return if secret_variable(name) {
                    Self::SecretVar
                } else {
                    Self::Other
                };
            }
            if let Some(&(_, secrets)) = TSQL_PROCEDURES
                .iter()
                .find(|(name, _)| word.eq_ignore_ascii_case(name.as_bytes()))
            {
                return Self::Proc(secrets);
            }
        }
        let is = |k: &str| word.eq_ignore_ascii_case(k.as_bytes());
        if is("PASSWORD") {
            Self::Secret { password: true }
        } else if is("IDENTIFIED")
            || is("MASTER_PASSWORD")
            || is("SOURCE_PASSWORD")
            || is("OLD_PASSWORD")
            || is("SECRET")
            || (tsql && TSQL_SECRET_WORDS.iter().any(|k| is(k)))
        {
            Self::Secret { password: false }
        } else if is("KEY") || is("REKEY") || is("HEXKEY") || is("HEXREKEY") {
            Self::Key
        } else if is("BY") || is("AS") || is("USING") {
            Self::Filler
        } else if is("WITH") || is("VIA") {
            Self::Plugin
        } else if is("FOR") {
            Self::For
        } else if is("REPLACE") {
            Self::Replace
        } else {
            Self::Other
        }
    }
}

/// T-SQL words that arm besides the common ones: `BACKUP ... MEDIAPASSWORD = '...'`,
/// `CREATE SYMMETRIC KEY ... KEY_SOURCE = '...'`, `ALTER SERVICE MASTER KEY ... NEW_PASSWORD =
/// '...'`, and the functions `PWDENCRYPT('...')` / `PWDCOMPARE('...', hash)`.
const TSQL_SECRET_WORDS: &[&str] = &[
    "MEDIAPASSWORD",
    "KEY_SOURCE",
    "NEW_PASSWORD",
    "PWDENCRYPT",
    "PWDCOMPARE",
];

/// Parts of T-SQL variable and argument names (after the `@`, case-insensitive) that make
/// the assigned value a secret.
const TSQL_SECRET_NAME_PARTS: &[&str] = &["password", "passwd", "pwd", "secret"];

/// T-SQL variable and argument names (after the `@`) that are secrets as a whole:
/// `sp_password @old = ..., @new = ...`.
const TSQL_SECRET_NAMES: &[&str] = &["pw", "pass", "new", "old"];

/// T-SQL system procedures and the positional arguments (bit `i`: argument `i`, from 0) that
/// hold a password.
const TSQL_PROCEDURES: &[(&str, u8)] = &[
    // @loginame, @passwd
    ("sp_addlogin", 0b10),
    // @old, @new
    ("sp_password", 0b11),
    // @rolename, @password
    ("sp_setapprole", 0b10),
    ("sp_addapprole", 0b10),
    // @rolename, @newpwd
    ("sp_approlepassword", 0b10),
    // @db_name, @password
    ("sp_control_dbmasterkey_password", 0b10),
    // @Action, @UserNamePattern, @LoginName, @Password
    ("sp_change_users_login", 0b1000),
    // @rmtsrvname, @useself, @locallogin, @rmtuser, @rmtpassword
    ("sp_addlinkedsrvlogin", 0b1_0000),
];

/// Whether a T-SQL variable or argument name (without its `@`) names a secret.
fn secret_variable(name: &[u8]) -> bool {
    TSQL_SECRET_NAMES
        .iter()
        .any(|k| name.eq_ignore_ascii_case(k.as_bytes()))
        || TSQL_SECRET_NAME_PARTS.iter().any(|part| {
            name.windows(part.len())
                .any(|w| w.eq_ignore_ascii_case(part.as_bytes()))
        })
}

/// Whether positional argument `arg` is a password in a [`State::Positional`] mask.
fn secret_arg(secrets: u8, arg: u8) -> bool {
    secrets
        .checked_shr(u32::from(arg))
        .is_some_and(|bits| bits & 1 == 1)
}

/// One replacement: `start..end` of the text becomes `text`.
type Edit = (usize, usize, Cow<'static, str>);

pub(crate) fn redact(dialect: SqlDialect, sql: &str) -> Cow<'_, str> {
    redact_at(dialect, sql, 0)
}

fn redact_at(dialect: SqlDialect, sql: &str, depth: u8) -> Cow<'_, str> {
    let mut rules = Rules::new(dialect);
    rules.open_exec_comments = true;
    let mut redactor = Redactor {
        rules,
        sql,
        depth,
        edits: Vec::new(),
        state: State::Off,
        head: Head::Start,
        after_eq: false,
    };
    let mut i = 0;
    while i < sql.len() {
        let tok = lex::token_at(sql, i, rules, &[]);
        i = tok.end;
        if matches!(
            tok.kind,
            TokenKind::Space | TokenKind::LineComment | TokenKind::BlockComment
        ) {
            continue;
        }
        let prev_eq = std::mem::replace(&mut redactor.after_eq, false);
        if redactor.head == Head::Start && tok.kind != TokenKind::Word {
            redactor.head = Head::Other;
        }
        redactor.state = match tok.kind {
            TokenKind::Str {
                inner_start,
                inner_end,
            } => redactor.on_str(tok.start, tok.end, inner_start, inner_end),
            TokenKind::Ident { quote } => redactor.on_ident(tok.start, tok.end, quote),
            TokenKind::Word => redactor.on_word(tok.start, tok.end, prev_eq),
            TokenKind::Punct => redactor.on_punct(tok.start, tok.end),
            // Not produced while executable comments are opened; nothing to arm on anyway.
            TokenKind::ExecComment
            | TokenKind::Space
            | TokenKind::LineComment
            | TokenKind::BlockComment => State::Off,
        };
    }
    apply(sql, redactor.edits)
}

/// The state of one redaction pass.
struct Redactor<'a> {
    rules: Rules,
    sql: &'a str,
    depth: u8,
    /// Replacements in order and not overlapping.
    edits: Vec<Edit>,
    state: State,
    head: Head,
    /// The previous content token was `=`.
    after_eq: bool,
}

impl Redactor<'_> {
    fn sqlite(&self) -> bool {
        self.rules.dialect == SqlDialect::Sqlite
    }

    fn tsql(&self) -> bool {
        self.rules.dialect == SqlDialect::TSql
    }

    /// Replaces `start..end` with [`REDACTED_LITERAL`].
    fn mask(&mut self, start: usize, end: usize) -> State {
        self.edits
            .push((start, end, Cow::Borrowed(REDACTED_LITERAL)));
        State::AfterSecret
    }

    /// [`Self::mask`] for a positional procedure argument: the next arguments still count.
    fn mask_argument(&mut self, start: usize, end: usize) -> State {
        let state = self.state;
        self.mask(start, end);
        state
    }

    fn on_str(&mut self, start: usize, end: usize, inner_start: usize, inner_end: usize) -> State {
        match self.state {
            State::Armed { .. } => return self.mask(start, end),
            State::Positional { secrets, arg } if secret_arg(secrets, arg) => {
                return self.mask_argument(start, end);
            }
            _ => {}
        }
        if let Some(content) = redact_literal(
            self.rules,
            self.sql,
            start,
            inner_start,
            inner_end,
            self.depth,
        ) {
            self.edits
                .push((inner_start, inner_end, Cow::Owned(content)));
        }
        match self.state {
            State::Plugin { paren } => State::Armed {
                paren,
                password: false,
            },
            State::ForUser => State::ForUser,
            State::Positional { .. } => self.state,
            _ => State::Off,
        }
    }

    fn on_ident(&mut self, start: usize, end: usize, quote: u8) -> State {
        let state = self.state;
        match state {
            // SQLCipher keys are often written `PRAGMA key = "x'...'"`.
            State::Armed { paren: false, .. } if self.sqlite() && quote == b'"' => {
                self.mask(start, end)
            }
            // T-SQL with `QUOTED_IDENTIFIER OFF`: `"..."` is a string.
            State::Armed { .. } if self.tsql() && quote == b'"' => self.mask(start, end),
            State::Positional { secrets, arg } => {
                if quote == b'"' && secret_arg(secrets, arg) {
                    self.mask_argument(start, end)
                } else {
                    state
                }
            }
            // `DECLARE @pwd [nvarchar](50)`, `@pwd [sys].[sysname]`.
            State::VarDecl {
                typed: false,
                open: false,
            } => State::VarDecl {
                typed: true,
                open: false,
            },
            State::VarDecl { open: true, .. } => state,
            State::Plugin { paren } => State::Armed {
                paren,
                password: false,
            },
            State::ForUser => State::ForUser,
            _ => State::Off,
        }
    }

    fn on_word(&mut self, start: usize, end: usize, prev_eq: bool) -> State {
        let sql = self.sql;
        let bytes = sql.as_bytes();
        let word = &bytes[start..end];
        if self.head == Head::Start {
            self.head =
                if word.eq_ignore_ascii_case(b"PRAGMA") || word.eq_ignore_ascii_case(b"ATTACH") {
                    Head::Keyed
                } else {
                    Head::Other
                };
        }
        let keyed = self.sqlite() && self.head == Head::Keyed;
        let kind = match Word::of(self.rules.dialect, word) {
            Word::Key if keyed => Word::Secret { password: false },
            Word::Key => Word::Other,
            kind => kind,
        };
        let state = self.state;
        match (state, kind) {
            (_, Word::Secret { password }) => State::Armed {
                paren: false,
                password,
            },
            (_, Word::SecretVar) => State::VarDecl {
                typed: false,
                open: false,
            },
            (_, Word::Proc(secrets)) => State::Positional { secrets, arg: 0 },
            (State::Positional { secrets, arg }, _) => {
                self.on_positional_word(secrets, arg, start, end)
            }
            // `DECLARE @pwd AS nvarchar(50) = ...`
            (
                State::VarDecl {
                    typed: false,
                    open: false,
                },
                Word::Filler,
            )
            | (State::VarDecl { open: true, .. }, _) => state,
            (
                State::VarDecl {
                    typed: false,
                    open: false,
                },
                _,
            ) => State::VarDecl {
                typed: true,
                open: false,
            },
            (State::Armed { paren, .. }, Word::Filler) | (State::Plugin { paren }, _) => {
                State::Armed {
                    paren,
                    password: false,
                }
            }
            (State::Armed { paren, .. }, Word::Plugin) => State::Plugin { paren },
            (State::Armed { password: true, .. }, Word::For) | (State::ForUser, _) => {
                State::ForUser
            }
            (State::Armed { .. }, _) if is_introducer(self.rules.dialect, bytes, end, word) => {
                state
            }
            // SQLCipher `PRAGMA key = passphrase`: a bare word is the key.
            (State::Armed { paren: false, .. }, _) if keyed && prev_eq => self.mask(start, end),
            // T-SQL `PASSWORD = 0x0200... HASHED`: the password hash.
            (State::Armed { paren: false, .. }, _)
                if prev_eq
                    && self.rules.dialect == SqlDialect::TSql
                    && word.len() > 2
                    && word[..2].eq_ignore_ascii_case(b"0x") =>
            {
                self.mask(start, end)
            }
            (State::AfterSecret, Word::Replace) => State::Armed {
                paren: false,
                password: false,
            },
            _ => State::Off,
        }
    }

    /// A word among the positional arguments of a [`TSQL_PROCEDURES`] call: `NULL`,
    /// `DEFAULT`, a number, a variable and the `N` of `N'...'` are arguments (a number in a
    /// password position is masked); any other word ends the call.
    fn on_positional_word(&mut self, secrets: u8, arg: u8, start: usize, end: usize) -> State {
        let sql = self.sql;
        let bytes = sql.as_bytes();
        let word = &bytes[start..end];
        let stay = State::Positional { secrets, arg };
        if is_introducer(self.rules.dialect, bytes, end, word)
            || word.eq_ignore_ascii_case(b"NULL")
            || word.eq_ignore_ascii_case(b"DEFAULT")
            || word.first() == Some(&b'@')
        {
            return stay;
        }
        if word.first().is_some_and(u8::is_ascii_digit) {
            return if secret_arg(secrets, arg) {
                self.mask_argument(start, end)
            } else {
                stay
            };
        }
        State::Off
    }

    fn on_punct(&mut self, start: usize, end: usize) -> State {
        let sql = self.sql;
        let bytes = sql.as_bytes();
        let c = bytes[start];
        self.after_eq = c == b'=';
        if c == b';' {
            self.head = Head::Start;
        }
        // PostgreSQL `U&'...'`: the `&` between the `U` and the quote.
        let unicode_amp = c == b'&'
            && self.rules.dialect == SqlDialect::Postgres
            && start > 0
            && bytes[start - 1] | 0x20 == b'u'
            && bytes.get(end) == Some(&b'\'');
        match (self.state, c) {
            (State::Armed { password, .. }, b'(') => State::Armed {
                paren: true,
                password,
            },
            (State::Armed { .. }, b'=' | b'&') if c == b'=' || unicode_amp => self.state,
            // `SET PASSWORD FOR u = '...'`, `@pwd nvarchar(50) = '...'`.
            (State::ForUser | State::VarDecl { open: false, .. }, b'=') => State::Armed {
                paren: false,
                password: false,
            },
            (State::ForUser, c) if c != b';' => State::ForUser,
            (
                State::VarDecl {
                    typed: true,
                    open: false,
                },
                b'(',
            ) => State::VarDecl {
                typed: true,
                open: true,
            },
            (State::VarDecl { open: true, .. }, b',') => self.state,
            (State::VarDecl { open: true, .. }, b')') => State::VarDecl {
                typed: true,
                open: false,
            },
            // `@pwd sys.sysname`
            (
                State::VarDecl {
                    typed: true,
                    open: false,
                },
                b'.',
            ) => State::VarDecl {
                typed: false,
                open: false,
            },
            (State::Positional { secrets, arg }, b',') => State::Positional {
                secrets,
                arg: arg.saturating_add(1),
            },
            _ => State::Off,
        }
    }
}

/// `sql` with `edits` applied; borrowed when there are none.
fn apply(sql: &str, edits: Vec<Edit>) -> Cow<'_, str> {
    if edits.is_empty() {
        return Cow::Borrowed(sql);
    }
    let mut out = String::with_capacity(sql.len());
    let mut last = 0;
    for (start, end, replacement) in edits {
        // Every edit boundary is next to an ASCII byte or at a token boundary.
        out.push_str(sql.get(last..start).unwrap_or_default());
        out.push_str(&replacement);
        last = end;
    }
    out.push_str(sql.get(last..).unwrap_or_default());
    Cow::Owned(out)
}

/// A literal that is not itself a secret: rule 2 over its content `start..end`, then the
/// content redacted again as SQL (with its quote escapes undone, and redone afterwards).
/// Returns the new content when something was masked.
fn redact_literal(
    rules: Rules,
    sql: &str,
    token_start: usize,
    start: usize,
    end: usize,
    depth: u8,
) -> Option<String> {
    let bytes = sql.as_bytes();
    // The content starts after an ASCII quote / dollar tag and ends at one or at the text end,
    // so it is on char boundaries.
    let content = sql.get(start..end)?;
    // `None`: a dollar-quoted body (no escapes). Otherwise the quote byte and whether a
    // backslash escapes.
    let escapes = match bytes[token_start] {
        b'$' => None,
        b'E' | b'e' => Some((b'\'', true)),
        quote => Some((quote, rules.backslash_escapes)),
    };
    let looks_like_code = escapes.is_none() && content.bytes().any(|b| b == b';' || b == b'\n');
    let mut current = Cow::Borrowed(content);
    if !looks_like_code {
        let keys = if rules.dialect == SqlDialect::Sqlite {
            SQLITE_VALUE_KEYS
        } else {
            VALUE_KEYS
        };
        let backslash = escapes.is_some_and(|(_, backslash)| backslash);
        let mut edits = Vec::new();
        connection_string_values(bytes, start, end, keys, backslash, &mut edits);
        if !edits.is_empty() {
            let local = edits
                .into_iter()
                .map(|(from, to, text)| (from - start, to - start, text))
                .collect();
            current = apply(content, local);
        }
    }
    if depth < MAX_DEPTH {
        let nested = match escapes {
            None => match redact_at(rules.dialect, &current, depth + 1) {
                Cow::Owned(text) => Some(text),
                Cow::Borrowed(_) => None,
            },
            Some((quote, backslash)) => {
                let plain = unescape(&current, quote, backslash);
                match redact_at(rules.dialect, &plain, depth + 1) {
                    Cow::Owned(text) => Some(escape(&text, quote)),
                    Cow::Borrowed(_) => None,
                }
            }
        };
        if let Some(text) = nested {
            current = Cow::Owned(text);
        }
    }
    match current {
        Cow::Owned(text) => Some(text),
        Cow::Borrowed(_) => None,
    }
}

/// The text a quoted literal's content stands for: a doubled quote and (with `backslash`) a
/// backslash-escaped quote become one quote; every other byte is kept.
fn unescape(content: &str, quote: u8, backslash: bool) -> Cow<'_, str> {
    let bytes = content.as_bytes();
    if !bytes
        .iter()
        .any(|&b| b == quote || (backslash && b == b'\\'))
    {
        return Cow::Borrowed(content);
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let next = bytes.get(i + 1).copied();
        if (b == quote || (backslash && b == b'\\')) && next == Some(quote) {
            out.push(quote);
            i += 2;
        } else if backslash && b == b'\\' {
            out.push(b);
            if let Some(next) = next {
                out.push(next);
            }
            i += 2;
        } else {
            out.push(b);
            i += 1;
        }
    }
    // Only ASCII pairs were merged, so the bytes are still UTF-8.
    String::from_utf8(out).map_or(Cow::Borrowed(content), Cow::Owned)
}

/// The inverse of [`unescape`] for text going back into a literal: every quote is doubled.
fn escape(text: &str, quote: u8) -> String {
    let quote = char::from(quote);
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        out.push(c);
        if c == quote {
            out.push(c);
        }
    }
    out
}

/// Rule 2: inside a literal's content `start..end`, masks the value after one of `keys`
/// (`password=`, `pwd=`, ...; case-insensitive, not inside a longer word, spaces allowed
/// around `=`) up to the next whitespace, `;`, `&` or the end of the content. A value starting
/// with quotes runs to the closing quote run; with `backslash`, a value starting with `\'`
/// runs to the next `\'`. Edits are pushed in byte offsets of the whole text.
fn connection_string_values(
    bytes: &[u8],
    start: usize,
    end: usize,
    keys: &[&[u8]],
    backslash: bool,
    edits: &mut Vec<Edit>,
) {
    let end = end.min(bytes.len());
    let mut k = start;
    while k < end {
        let first = bytes[k] | 0x20;
        if !keys.iter().any(|key| key[0] == first) {
            k += 1;
            continue;
        }
        let rest = &bytes[k..end];
        let key_len = keys
            .iter()
            .find(|key| starts_with_ignore_case(rest, key))
            .map_or(0, |key| key.len());
        let in_word = k > start && is_key_byte(bytes[k - 1]);
        if key_len == 0 || in_word {
            k += 1;
            continue;
        }
        let mut v = skip_blanks(bytes, k + key_len, end);
        if v >= end || bytes[v] != b'=' {
            k += key_len;
            continue;
        }
        v = skip_blanks(bytes, v + 1, end);
        let stop = value_end(bytes, v, end, backslash);
        if stop > v {
            edits.push((v, stop, Cow::Borrowed(REDACTED_VALUE)));
        }
        k = stop.max(v);
    }
}

fn starts_with_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .get(..needle.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(needle))
}

/// A literal prefix written as a separate word: a MySQL charset introducer (`_utf8mb4'...'`),
/// a one-letter prefix directly before the quote (`N'...'`, `X'...'`, `B'...'`), or the `U` of
/// PostgreSQL `U&'...'`.
fn is_introducer(dialect: SqlDialect, bytes: &[u8], word_end: usize, word: &[u8]) -> bool {
    let next = bytes.get(word_end);
    word.first() == Some(&b'_')
        || (matches!(word, [b'N' | b'n' | b'X' | b'x' | b'B' | b'b']) && next == Some(&b'\''))
        || (dialect == SqlDialect::Postgres
            && matches!(word, [b'U' | b'u'])
            && next == Some(&b'&')
            && bytes.get(word_end + 1) == Some(&b'\''))
}

const fn is_key_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn skip_blanks(bytes: &[u8], mut i: usize, end: usize) -> usize {
    while i < end && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    i
}

fn value_end(bytes: &[u8], v: usize, end: usize, backslash: bool) -> usize {
    let Some(&first) = bytes.get(v).filter(|_| v < end) else {
        return v;
    };
    let mut j = v;
    if first == b'\'' || first == b'"' {
        while j < end && bytes[j] == first {
            j += 1;
        }
        while j < end && bytes[j] != first {
            j += 1;
        }
        while j < end && bytes[j] == first {
            j += 1;
        }
        return j;
    }
    // MySQL `password=\'a b\'` inside a `'...'` literal.
    if backslash
        && first == b'\\'
        && let Some(&quote) = bytes.get(v + 1).filter(|&&q| q == b'\'' || q == b'"')
    {
        j = v + 2;
        while j < end {
            if bytes[j] == b'\\' {
                if bytes.get(j + 1) == Some(&quote) {
                    return (j + 2).min(end);
                }
                j += 2;
            } else {
                j += 1;
            }
        }
        return end;
    }
    while j < end && !matches!(bytes[j], b';' | b'&') && !lex::is_space(bytes[j]) {
        j += 1;
    }
    j
}
