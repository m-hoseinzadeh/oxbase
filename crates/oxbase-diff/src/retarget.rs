//! Moving names inside engine-native SQL text between containers (`docs/m8-contracts.md`
//! §3.2): the part of relocation `oxbase_types::relocate_def` leaves alone. Owner of the body:
//! the **structure-sync** stream (used by structure sync and by restore into another
//! container).
//!
//! Rules for [`retarget_sql`]:
//!
//! - Tokens come from `oxbase_sql::tokenize` with the engine's `SqlDialect`, so names inside
//!   strings and comments are never touched.
//! - An identifier (quoted or not) that names the source schema (PostgreSQL, SQL Server) or the
//!   source database (MySQL, MariaDB; also a SQL Server three-part name's database part) and is
//!   **followed by `.`** is replaced by the target's name, quoted the way the source spelled it
//!   (a quoted identifier stays quoted, with the engine's quoting; an unquoted one stays
//!   unquoted when the target name needs no quotes, else it is quoted).
//! - The comparison follows the engine's identifier rules: unquoted PostgreSQL identifiers fold
//!   to lower case, MySQL / MariaDB / SQL Server compare case-insensitively, quoted identifiers
//!   exactly.
//! - Nothing else changes (no reformatting): text without the source name comes back equal.
//! - SQLite: no schemas to move (`main` stays `main`); the text is returned unchanged.

use oxbase_sql::{SqlDialect, SqlToken, SqlTokenKind, ident_value, tokenize};
use oxbase_types::{Engine, Location};

/// `text` (engine-native SQL of `engine`) with every qualified reference into `from` moved to
/// `to` (module docs).
pub fn retarget_sql(engine: Engine, text: &str, from: &Location, to: &Location) -> String {
    let Some(dialect) = SqlDialect::for_engine(engine) else {
        return text.to_owned();
    };
    if from == to || dialect == SqlDialect::Sqlite {
        return text.to_owned();
    }
    let names = Names::new(dialect, from, to);
    if names.schema.is_none() && names.catalog.is_none() {
        return text.to_owned();
    }
    let tokens: Vec<SqlToken> = tokenize(dialect, text).collect();
    let mut out = String::with_capacity(text.len() + 16);
    let mut at = 0;
    for (pos, token) in tokens.iter().enumerate() {
        let replacement = match token.kind {
            SqlTokenKind::Word | SqlTokenKind::QuotedIdent { .. } => {
                names.qualifier(text, &tokens, pos)
            }
            SqlTokenKind::String {
                inner_start,
                inner_end,
            } if dialect == SqlDialect::Postgres && regclass_follows(text, &tokens, pos) => {
                let prefix = &text[token.start..inner_start];
                // `E'...'` reads backslashes as escapes.
                let escapes = prefix.starts_with(['E', 'e']);
                names
                    .regclass(&text[inner_start..inner_end], escapes)
                    .map(|inner| format!("{prefix}{inner}{}", &text[inner_end..token.end]))
            }
            _ => None,
        };
        if let Some(new_text) = replacement {
            out.push_str(&text[at..token.start]);
            out.push_str(&new_text);
            at = token.end;
        }
    }
    out.push_str(&text[at..]);
    out
}

/// What moves: the schema (engines with schemas) and the catalog (a database name in front of
/// a schema, or the qualifier itself on engines without schemas).
struct Names<'a> {
    dialect: SqlDialect,
    /// `(from, to)` schema names.
    schema: Option<(&'a str, &'a str)>,
    /// `(from, to)` catalog names.
    catalog: Option<(&'a str, &'a str)>,
    /// The engine has schemas: a catalog then only qualifies a schema (`db.schema.table`).
    has_schemas: bool,
}

impl<'a> Names<'a> {
    fn new(dialect: SqlDialect, from: &'a Location, to: &'a Location) -> Self {
        let schema = match (&from.schema, &to.schema) {
            (Some(a), Some(b)) if a != b => Some((a.as_str(), b.as_str())),
            _ => None,
        };
        let catalog = match (&from.catalog, &to.catalog) {
            (Some(a), Some(b)) if a != b => Some((a.as_str(), b.as_str())),
            _ => None,
        };
        Self {
            dialect,
            schema,
            catalog,
            has_schemas: from.schema.is_some(),
        }
    }

    /// `true` when the token text names `name` under the engine's identifier rules.
    fn matches(&self, token: &str, quoted: bool, name: &str) -> bool {
        if quoted {
            return match self.dialect {
                SqlDialect::Postgres => ident_value(token) == name,
                _ => ident_value(token).eq_ignore_ascii_case(name),
            };
        }
        match self.dialect {
            // Unquoted names fold to lower case.
            SqlDialect::Postgres => token.to_lowercase() == name,
            _ => token.eq_ignore_ascii_case(name),
        }
    }

    /// The new text of the identifier token at `pos` when it is a qualifier into the source
    /// container (followed by `.`).
    fn qualifier(&self, text: &str, tokens: &[SqlToken], pos: usize) -> Option<String> {
        let token = tokens[pos];
        let token_text = token.text(text);
        let quoted = matches!(token.kind, SqlTokenKind::QuotedIdent { .. });
        let dot = next_significant(tokens, pos)?;
        if tokens[dot].text(text) != "." {
            return None;
        }
        // A part after a `.` is never the first part of a name we move, unless it is the
        // schema of a three-part name (`db.schema.table`).
        let after_dot =
            previous_significant(tokens, pos).is_some_and(|p| tokens[p].text(text) == ".");
        let three_part = next_significant(tokens, dot)
            .and_then(|name| next_significant(tokens, name))
            .is_some_and(|d| tokens[d].text(text) == ".");
        if let Some((from, to)) = self.schema
            && self.matches(token_text, quoted, from)
        {
            return Some(self.spell(token_text, quoted, to));
        }
        if after_dot {
            return None;
        }
        if let Some((from, to)) = self.catalog
            && (three_part || !self.has_schemas)
            && self.matches(token_text, quoted, from)
        {
            return Some(self.spell(token_text, quoted, to));
        }
        None
    }

    /// `name` written the way `original` was: quoted with the same quote, or plain when the
    /// name needs no quotes.
    fn spell(&self, original: &str, quoted: bool, name: &str) -> String {
        let open = if quoted {
            original.as_bytes().first().copied().unwrap_or(b'"')
        } else if plain(self.dialect, name) {
            return name.to_owned();
        } else {
            match self.dialect {
                SqlDialect::MySql { .. } => b'`',
                SqlDialect::TSql => b'[',
                SqlDialect::Postgres | SqlDialect::Sqlite => b'"',
            }
        };
        quote(open, name)
    }

    /// The inner text of a PostgreSQL `'schema.name'::regclass` literal with the schema moved.
    /// The new schema name is escaped for the string literal (quotes doubled; backslashes too
    /// in an `E'...'` string), so no target name can end the literal early.
    fn regclass(&self, inner: &str, escapes: bool) -> Option<String> {
        let (from, to) = self.schema?;
        let parts = split_path(inner);
        if parts.len() < 2 {
            return None;
        }
        let first = parts[0];
        let quoted = first.starts_with('"');
        // The literal's own escapes undone: `'it''s'` holds `it's`.
        let unescaped = first.replace("''", "'");
        if !self.matches(&unescaped, quoted, from) {
            return None;
        }
        let rest = &inner[first.len()..];
        let spelled = self.spell(&unescaped, quoted, to);
        let mut literal = String::with_capacity(spelled.len() + 2);
        for c in spelled.chars() {
            if c == '\'' || (escapes && c == '\\') {
                literal.push(c);
            }
            literal.push(c);
        }
        Some(format!("{literal}{rest}"))
    }
}

/// `true` when `name` can be written without quotes on the dialect (no case folding changes
/// it, no special characters).
fn plain(dialect: SqlDialect, name: &str) -> bool {
    let Some(first) = name.chars().next() else {
        return false;
    };
    let ok_start = first == '_' || first.is_ascii_alphabetic();
    let ok_rest = name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric());
    let case_ok = match dialect {
        SqlDialect::Postgres => !name.chars().any(|c| c.is_ascii_uppercase()),
        _ => true,
    };
    ok_start && ok_rest && case_ok
}

fn quote(open: u8, name: &str) -> String {
    let (open, close) = match open {
        b'`' => ('`', '`'),
        b'[' => ('[', ']'),
        _ => ('"', '"'),
    };
    let mut out = String::with_capacity(name.len() + 2);
    out.push(open);
    for c in name.chars() {
        if c == close {
            out.push(close);
        }
        out.push(c);
    }
    out.push(close);
    out
}

/// The dotted parts of a name inside a string (`a.b`, `"A"."b"`), quotes kept.
fn split_path(inner: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    for (i, c) in inner.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            '.' if !in_quotes => {
                parts.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&inner[start..]);
    parts
}

fn next_significant(tokens: &[SqlToken], pos: usize) -> Option<usize> {
    (pos + 1..tokens.len()).find(|&i| !tokens[i].is_trivia())
}

fn previous_significant(tokens: &[SqlToken], pos: usize) -> Option<usize> {
    (0..pos).rev().find(|&i| !tokens[i].is_trivia())
}

/// `true` when the string token at `pos` is followed by `::regclass`.
fn regclass_follows(text: &str, tokens: &[SqlToken], pos: usize) -> bool {
    let Some(first) = next_significant(tokens, pos) else {
        return false;
    };
    let Some(second) = next_significant(tokens, first) else {
        return false;
    };
    let Some(word) = next_significant(tokens, second) else {
        return false;
    };
    tokens[first].text(text) == ":"
        && tokens[second].text(text) == ":"
        && tokens[word].text(text).eq_ignore_ascii_case("regclass")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn pg(text: &str) -> String {
        retarget_sql(
            Engine::Postgres,
            text,
            &Location::new(Some("oxbase_test"), Some("m8_src")),
            &Location::new(Some("oxbase_test"), Some("m8_tgt")),
        )
    }

    #[test]
    fn postgres_moves_qualifiers_only() {
        assert_eq!(
            pg("SELECT c.id FROM m8_src.customers c JOIN \"m8_src\".\"Orders\" o ON true"),
            "SELECT c.id FROM m8_tgt.customers c JOIN \"m8_tgt\".\"Orders\" o ON true"
        );
        // Unquoted names fold: M8_SRC is m8_src. A quoted "M8_SRC" is another schema.
        assert_eq!(
            pg("SELECT * FROM M8_SRC.t, \"M8_SRC\".t"),
            "SELECT * FROM m8_tgt.t, \"M8_SRC\".t"
        );
        // Strings, comments, dollar quotes and unqualified names stay.
        let untouched =
            "SELECT 'm8_src.t', m8_src -- m8_src.t\n /* m8_src.x */ FROM t; $$m8_src.t$$";
        assert_eq!(pg(untouched), untouched);
        // A three-part name and a column of a table that happens to be called like the schema.
        assert_eq!(
            pg("SELECT oxbase_test.m8_src.t.a, x.m8_src FROM oxbase_test.m8_src.t"),
            "SELECT oxbase_test.m8_tgt.t.a, x.m8_src FROM oxbase_test.m8_tgt.t"
        );
        // regclass literals name a relation.
        assert_eq!(
            pg("nextval('m8_src.invoice_seq'::regclass)"),
            "nextval('m8_tgt.invoice_seq'::regclass)"
        );
        assert_eq!(pg("'m8_src.x'::text"), "'m8_src.x'::text");
    }

    fn pg_to(target: &str, text: &str) -> String {
        retarget_sql(
            Engine::Postgres,
            text,
            &Location::new(None, Some("m8_src")),
            &Location::new(None, Some(target)),
        )
    }

    #[test]
    fn regclass_literals_escape_the_target_name() {
        // A quote in the target name is doubled inside the literal (and quoted as a name).
        assert_eq!(
            pg_to("x'y", "nextval('m8_src.seq'::regclass)"),
            "nextval('\"x''y\".seq'::regclass)"
        );
        assert_eq!(pg_to("x'y", "SELECT m8_src.f()"), "SELECT \"x'y\".f()");
        // A double quote is doubled as part of the quoted name only.
        assert_eq!(
            pg_to("a\"b", "nextval('m8_src.seq'::regclass)"),
            "nextval('\"a\"\"b\".seq'::regclass)"
        );
        // A backslash is plain in a standard string, doubled in an E'' string.
        assert_eq!(
            pg_to("a\\b", "nextval('m8_src.seq'::regclass)"),
            "nextval('\"a\\b\".seq'::regclass)"
        );
        assert_eq!(
            pg_to("a\\b", "nextval(E'm8_src.seq'::regclass)"),
            "nextval(E'\"a\\\\b\".seq'::regclass)"
        );
        // An attempt to close the literal stays inside it.
        let out = pg_to("t'); DROP TABLE x; --", "nextval('m8_src.seq'::regclass)");
        assert_eq!(out, "nextval('\"t''); DROP TABLE x; --\".seq'::regclass)");
    }

    #[test]
    fn postgres_quotes_a_target_that_needs_it() {
        let out = retarget_sql(
            Engine::Postgres,
            "SELECT * FROM m8_src.t",
            &Location::new(None, Some("m8_src")),
            &Location::new(None, Some("Target Schema")),
        );
        assert_eq!(out, "SELECT * FROM \"Target Schema\".t");
    }

    #[test]
    fn mysql_moves_databases_case_insensitively() {
        let from = Location::new(Some("m8_src"), None);
        let to = Location::new(Some("m8_tgt"), None);
        assert_eq!(
            retarget_sql(
                Engine::MySql,
                "select `m8_src`.`customers`.`id` AS `id` from `M8_SRC`.`customers` where name = 'm8_src.x' # m8_src.y",
                &from,
                &to
            ),
            "select `m8_tgt`.`customers`.`id` AS `id` from `m8_tgt`.`customers` where name = 'm8_src.x' # m8_src.y"
        );
        assert_eq!(
            retarget_sql(
                Engine::MariaDb,
                "INSERT INTO m8_src.audit VALUES (1)",
                &from,
                &to
            ),
            "INSERT INTO m8_tgt.audit VALUES (1)"
        );
    }

    #[test]
    fn sql_server_moves_schemas_and_three_part_databases() {
        let from = Location::new(Some("db1"), Some("m8_src"));
        let to = Location::new(Some("db2"), Some("m8_tgt"));
        assert_eq!(
            retarget_sql(
                Engine::MsSql,
                "CREATE VIEW [m8_src].[v] AS SELECT * FROM M8_SRC.t JOIN db1.m8_src.u ON 1 = 1 WHERE x = N'm8_src.t'",
                &from,
                &to
            ),
            "CREATE VIEW [m8_tgt].[v] AS SELECT * FROM m8_tgt.t JOIN db2.m8_tgt.u ON 1 = 1 WHERE x = N'm8_src.t'"
        );
    }

    #[test]
    fn sqlite_and_same_locations_are_unchanged() {
        let text = "CREATE VIEW v AS SELECT * FROM main.t";
        let main = Location::new(Some("main"), None);
        assert_eq!(
            retarget_sql(
                Engine::Sqlite,
                text,
                &main,
                &Location::new(Some("aux"), None)
            ),
            text
        );
        assert_eq!(retarget_sql(Engine::MySql, text, &main, &main), text);
        assert_eq!(
            retarget_sql(Engine::Redis, text, &main, &Location::default()),
            text
        );
    }
}
