//! M6: [`schema_changes`], which tells the schema cache that a statement the user ran changed
//! the schema (`docs/m6-contracts.md` §3.5).

use crate::SqlDialect;
use crate::tokens::{SqlToken, SqlTokenKind, ident_value, tokenize};

/// The DDL verb of a [`SchemaChange`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DdlVerb {
    Create,
    Alter,
    Drop,
    /// `RENAME TABLE` (MySQL), `ALTER ... RENAME TO <new>` (the change names the new name),
    /// T-SQL `sp_rename` (no name).
    Rename,
}

/// The object kind a [`SchemaChange`] touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DdlObject {
    Table,
    View,
    Index,
    Schema,
    Database,
    Function,
    Procedure,
    Sequence,
    /// Types and domains.
    Type,
    Trigger,
    /// Anything else that can change names (extensions, synonyms, events, `sp_rename`): the
    /// cache refreshes everything.
    Other,
}

/// One part of a qualified name as written: its value without quotes, and whether it was
/// quoted (case rules differ).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NamePart {
    pub text: String,
    pub quoted: bool,
}

/// One schema change found in a statement.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SchemaChange {
    pub verb: DdlVerb,
    pub object: DdlObject,
    /// The object's name as written (`schema.table` = two parts). Empty when it could not be
    /// read: the cache then refreshes everything.
    pub name: Vec<NamePart>,
}

/// The schema changes a statement (or a T-SQL batch) probably makes. Best effort, lexical:
///
/// - candidates are the words `CREATE`, `ALTER`, `DROP` and `RENAME` outside strings and
///   comments (and T-SQL `sp_rename`), followed by optional modifiers (`OR REPLACE`,
///   `OR ALTER`, `TEMP`, `UNIQUE`, `MATERIALIZED`, MySQL `DEFINER = ...`, ...) and an object
///   word (`TABLE`, `VIEW`, `INDEX`, `SCHEMA`, `DATABASE`, `FUNCTION`, `PROCEDURE` / `PROC`,
///   `SEQUENCE`, `TYPE` / `DOMAIN`, `TRIGGER`, `EXTENSION`, `EVENT`, `SYNONYM`,
///   `AGGREGATE`); words such as `ALTER TABLE t DROP COLUMN c`'s `DROP COLUMN` match no object
///   word and are skipped;
/// - the name after the object word (and after `IF [NOT] EXISTS`, `CONCURRENTLY`) is read as
///   a dotted path; `RENAME [TO|AS] <name>` inside the statement adds a `Rename` change with
///   the new name;
/// - after a routine, trigger or event, scanning stops: the rest of the text is its body
///   (MySQL `BEGIN ... END`, a T-SQL procedure runs to the end of its batch), whose DDL runs
///   only when the routine is called.
///
/// Not detected: `SELECT ... INTO new_table`, DDL run by dynamic SQL or inside functions, and
/// executable MySQL comments. A false positive only costs a cache refresh; a miss leaves the
/// cache stale until the next refresh (the editor offers one).
pub fn schema_changes(dialect: SqlDialect, statement: &str) -> Vec<SchemaChange> {
    let toks: Vec<SqlToken> = tokenize(dialect, statement)
        .filter(|t| !t.is_trivia() && t.kind != SqlTokenKind::ExecComment)
        .collect();
    let word = |i: usize| -> Option<String> {
        toks.get(i)
            .filter(|t| t.kind == SqlTokenKind::Word)
            .map(|t| t.text(statement).to_ascii_uppercase())
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let Some(w) = word(i) else {
            i += 1;
            continue;
        };
        let verb = match w.as_str() {
            "CREATE" => DdlVerb::Create,
            "ALTER" => DdlVerb::Alter,
            "DROP" => DdlVerb::Drop,
            "RENAME" => DdlVerb::Rename,
            "SP_RENAME" => {
                out.push(SchemaChange {
                    verb: DdlVerb::Rename,
                    object: DdlObject::Other,
                    name: Vec::new(),
                });
                i += 1;
                continue;
            }
            _ => {
                i += 1;
                continue;
            }
        };
        // `RENAME TO new` / `RENAME AS new` inside an ALTER: the new name of the table.
        if verb == DdlVerb::Rename && matches!(word(i + 1).as_deref(), Some("TO" | "AS")) {
            let (name, next) = read_name(statement, &toks, i + 2);
            if !name.is_empty() {
                out.push(SchemaChange {
                    verb,
                    object: DdlObject::Table,
                    name,
                });
            }
            i = next.max(i + 1);
            continue;
        }
        let mut j = i + 1;
        let mut object = None;
        // Modifiers between the verb and the object word (at most a few tokens).
        while j < toks.len() && j <= i + 12 {
            if let Some(o) = word(j).as_deref().and_then(object_word) {
                object = Some(o);
                break;
            }
            let w = word(j);
            let modifier = matches!(
                w.as_deref(),
                Some(
                    "OR" | "REPLACE" | "TEMP" | "TEMPORARY" | "UNLOGGED" | "GLOBAL" | "LOCAL"
                        | "UNIQUE" | "CLUSTERED" | "NONCLUSTERED" | "MATERIALIZED" | "RECURSIVE"
                        | "ALGORITHM" | "UNDEFINED" | "MERGE" | "TEMPTABLE" | "SQL" | "SECURITY"
                        | "DEFINER" | "INVOKER" | "CURRENT_USER" | "FOREIGN" | "COLUMNSTORE"
                        | "FULLTEXT" | "SPATIAL" | "VIRTUAL" | "AGGREGATE"
                )
            )
                // T-SQL `CREATE OR ALTER`.
                || (w.as_deref() == Some("ALTER") && verb == DdlVerb::Create)
                // `=`, `@` and quoted parts of `DEFINER = user@host`, `ALGORITHM = MERGE`.
                || toks[j].kind != SqlTokenKind::Word
                || (j > i + 1 && is_definer_value(statement, &toks, j));
            if !modifier {
                break;
            }
            j += 1;
        }
        let Some(object) = object else {
            i += 1;
            continue;
        };
        let mut k = j + 1;
        while matches!(
            word(k).as_deref(),
            Some("IF" | "NOT" | "EXISTS" | "CONCURRENTLY")
        ) {
            k += 1;
        }
        let (name, mut next) = read_name(statement, &toks, k);
        out.push(SchemaChange { verb, object, name });
        // `DROP TABLE a, b` and `RENAME TABLE a TO b, c TO d`.
        loop {
            let after = toks.get(next).map(|t| t.text(statement));
            let renamed_to = verb == DdlVerb::Rename && word(next).as_deref() == Some("TO");
            if !(renamed_to || (after == Some(",") && verb != DdlVerb::Create)) {
                break;
            }
            let (more, after_more) = read_name(statement, &toks, next + 1);
            if more.is_empty() {
                break;
            }
            out.push(SchemaChange {
                verb,
                object,
                name: more,
            });
            next = after_more;
        }
        if matches!(
            object,
            DdlObject::Function | DdlObject::Procedure | DdlObject::Trigger
        ) || (object == DdlObject::Other && word(j).as_deref() == Some("EVENT"))
        {
            break;
        }
        i = next.max(j + 1);
    }
    out
}

fn object_word(w: &str) -> Option<DdlObject> {
    Some(match w {
        "TABLE" => DdlObject::Table,
        "VIEW" => DdlObject::View,
        "INDEX" => DdlObject::Index,
        "SCHEMA" => DdlObject::Schema,
        "DATABASE" => DdlObject::Database,
        "FUNCTION" => DdlObject::Function,
        "PROCEDURE" | "PROC" => DdlObject::Procedure,
        "SEQUENCE" => DdlObject::Sequence,
        "TYPE" | "DOMAIN" => DdlObject::Type,
        "TRIGGER" => DdlObject::Trigger,
        "EXTENSION" | "EVENT" | "SYNONYM" => DdlObject::Other,
        _ => return None,
    })
}

/// The tokens of a MySQL `DEFINER = user@host` value (`user`, `@`, `host`, `CURRENT_USER`).
fn is_definer_value(text: &str, toks: &[SqlToken], j: usize) -> bool {
    let prev = |n: usize| toks.get(j.wrapping_sub(n)).map(|t| t.text(text));
    matches!(prev(1), Some("=" | "@")) || (prev(2) == Some("=") && prev(1) == Some("@"))
}

/// A dotted name starting at token `start`; returns it and the index after it.
fn read_name(text: &str, toks: &[SqlToken], start: usize) -> (Vec<NamePart>, usize) {
    let mut parts = Vec::new();
    let mut i = start;
    while let Some(t) = toks.get(i) {
        let part = match t.kind {
            SqlTokenKind::Word => NamePart {
                text: t.text(text).to_owned(),
                quoted: false,
            },
            SqlTokenKind::QuotedIdent { .. } => NamePart {
                text: ident_value(t.text(text)),
                quoted: true,
            },
            _ => break,
        };
        parts.push(part);
        i += 1;
        if toks.get(i).map(|t| t.text(text)) == Some(".") {
            i += 1;
        } else {
            break;
        }
    }
    (parts, i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use DdlObject as O;
    use DdlVerb as V;

    const NOTHING: Vec<(DdlVerb, DdlObject, String)> = Vec::new();

    fn names(dialect: SqlDialect, sql: &str) -> Vec<(DdlVerb, DdlObject, String)> {
        schema_changes(dialect, sql)
            .into_iter()
            .map(|c| {
                let name: Vec<String> = c
                    .name
                    .iter()
                    .map(|p| {
                        if p.quoted {
                            format!("\"{}\"", p.text)
                        } else {
                            p.text.clone()
                        }
                    })
                    .collect();
                (c.verb, c.object, name.join("."))
            })
            .collect()
    }

    #[test]
    fn plain_statements_change_nothing() {
        let pg = SqlDialect::Postgres;
        assert_eq!(names(pg, "SELECT * FROM t"), NOTHING);
        assert_eq!(
            names(pg, "INSERT INTO t VALUES ('CREATE TABLE x')"),
            NOTHING
        );
        assert_eq!(names(pg, "-- DROP TABLE t\nSELECT 1"), NOTHING);
        assert_eq!(names(pg, "GRANT ALTER ON t TO u"), NOTHING);
    }

    #[test]
    fn creates_alters_and_drops() {
        let pg = SqlDialect::Postgres;
        assert_eq!(
            names(pg, "create table if not exists app.\"Orders\" (id int)"),
            [(V::Create, O::Table, "app.\"Orders\"".to_owned())]
        );
        assert_eq!(
            names(pg, "CREATE OR REPLACE MATERIALIZED VIEW v AS SELECT 1"),
            [(V::Create, O::View, "v".to_owned())]
        );
        assert_eq!(
            names(pg, "CREATE UNIQUE INDEX CONCURRENTLY ix ON t (a)"),
            [(V::Create, O::Index, "ix".to_owned())]
        );
        assert_eq!(
            names(pg, "ALTER TABLE t DROP COLUMN c, ADD COLUMN d int"),
            [(V::Alter, O::Table, "t".to_owned())]
        );
        assert_eq!(
            names(pg, "ALTER TABLE t RENAME TO u"),
            [
                (V::Alter, O::Table, "t".to_owned()),
                (V::Rename, O::Table, "u".to_owned())
            ]
        );
        assert_eq!(
            names(pg, "DROP TABLE IF EXISTS a, b"),
            [
                (V::Drop, O::Table, "a".to_owned()),
                (V::Drop, O::Table, "b".to_owned())
            ]
        );
        assert_eq!(
            names(
                pg,
                "CREATE FUNCTION f() RETURNS int AS $$ CREATE TABLE x $$ LANGUAGE sql"
            ),
            [(V::Create, O::Function, "f".to_owned())]
        );
        assert_eq!(
            names(pg, "CREATE EXTENSION hstore"),
            [(V::Create, O::Other, "hstore".to_owned())]
        );
    }

    #[test]
    fn mysql_forms() {
        let my = SqlDialect::MySql { mariadb: false };
        assert_eq!(
            names(my, "RENAME TABLE a TO b"),
            [
                (V::Rename, O::Table, "a".to_owned()),
                (V::Rename, O::Table, "b".to_owned())
            ]
        );
        assert_eq!(
            names(
                my,
                "CREATE DEFINER=`root`@`%` PROCEDURE p() BEGIN CREATE TABLE x (a int); END"
            ),
            [(V::Create, O::Procedure, "p".to_owned())]
        );
        assert_eq!(
            names(my, "CREATE ALGORITHM = MERGE VIEW `v` AS SELECT 1"),
            [(V::Create, O::View, "\"v\"".to_owned())]
        );
    }

    #[test]
    fn tsql_batches() {
        let ms = SqlDialect::TSql;
        assert_eq!(
            names(
                ms,
                "IF OBJECT_ID('dbo.t') IS NOT NULL DROP TABLE dbo.t\nCREATE TABLE [dbo].[t] (a int)"
            ),
            [
                (V::Drop, O::Table, "dbo.t".to_owned()),
                (V::Create, O::Table, "\"dbo\".\"t\"".to_owned())
            ]
        );
        assert_eq!(
            names(ms, "CREATE OR ALTER PROC dbo.p AS BEGIN DROP TABLE x END"),
            [(V::Create, O::Procedure, "dbo.p".to_owned())]
        );
        assert_eq!(
            names(ms, "EXEC sp_rename 'a', 'b'"),
            [(V::Rename, O::Other, String::new())]
        );
    }
}
