//! Fresh copies of the M8 seed (`docs/m8-contracts.md` §9) in scratch containers, and the
//! engine-specific SQL the harness needs around them (container DDL, stale scratch listing,
//! qualified names).
//!
//! The Docker init runs the seed scripts once, into the canonical containers (`m8_src`,
//! `m8_tgt`, ...). The live tests of the other streams change some of them (a sync applies to
//! `m8_tgt`), so the harness never uses them: it builds its own copies from the same scripts.
//! A networked seed marks the statements of the sync pair (and, on PostgreSQL, the transfer
//! set) with
//!
//! ```text
//! -- m8:pair begin
//! ...statements naming m8_src / m8_tgt...
//! -- m8:pair end
//! ```
//!
//! and [`section`] + [`rename`] turn that text into the same objects under scratch names (whole
//! words only, so `m8_xfer_target` is not touched by a rename of `m8_xfer`). SQLite has no
//! markers: each of its two scripts builds one file.
//!
//! Scratch names are `<prefix><token>_<role>` ([`ScratchNames`]), with the round-trip token of
//! [`crate::roundtrip::new_token`] (unix seconds + process id), so a later run can tell stale
//! containers ([`is_stale`]) and drop them.

use oxbase_types::{Engine, ObjectRef};

use crate::roundtrip;

/// Default `[m8] scratch_prefix`.
pub const DEFAULT_PREFIX: &str = "m8_t_";
/// Default `[m8] stale_after_secs`.
pub const DEFAULT_STALE_AFTER_SECS: u64 = 3600;

/// The canonical container names the seed scripts use (replaced by [`rename`]).
pub const CANONICAL_SOURCE: &str = "m8_src";
pub const CANONICAL_TARGET: &str = "m8_tgt";
pub const CANONICAL_TRANSFER_SOURCE: &str = "m8_xfer";

/// The lines between `-- m8:<name> begin` and `-- m8:<name> end` (markers excluded), or `None`
/// when the script has no complete section of that name.
pub fn section<'a>(script: &'a str, name: &str) -> Option<&'a str> {
    let begin = format!("-- m8:{name} begin");
    let end = format!("-- m8:{name} end");
    let mut start = None;
    let mut offset = 0;
    for line in script.split_inclusive('\n') {
        let trimmed = line.trim();
        if start.is_none() && trimmed == begin {
            start = Some(offset + line.len());
        } else if let Some(from) = start
            && trimmed == end
        {
            return script.get(from..offset);
        }
        offset += line.len();
    }
    None
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// `text` with every whole-word `from` replaced by `to` (a word is ASCII letters, digits, `_`
/// and `$`), so `m8_src` matches in `m8_src.t`, `` `m8_src` `` and `N'm8_src'` but not in
/// `m8_src_x`.
pub fn rename_word(text: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return text.to_owned();
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut i = 0;
    while let Some(pos) = text[i..].find(from) {
        let at = i + pos;
        let end = at + from.len();
        let before_ok = at == 0 || !is_word_byte(bytes[at - 1]);
        let after_ok = end == bytes.len() || !is_word_byte(bytes[end]);
        if before_ok && after_ok {
            out.push_str(&text[last..at]);
            out.push_str(to);
            last = end;
        }
        i = end;
    }
    out.push_str(&text[last..]);
    out
}

/// [`rename_word`] for every `(from, to)` pair, in order.
pub fn rename(text: &str, pairs: &[(&str, &str)]) -> String {
    pairs.iter().fold(text.to_owned(), |acc, (from, to)| {
        rename_word(&acc, from, to)
    })
}

/// The statements of `script` for `engine` (the `oxbase_sql` splitter: `;`, MySQL `DELIMITER`,
/// T-SQL `GO` batches), without empty ones.
pub fn statements(engine: Engine, script: &str) -> Vec<String> {
    let Some(dialect) = oxbase_sql::SqlDialect::for_engine(engine) else {
        return Vec::new();
    };
    oxbase_sql::split(dialect, script)
        .into_iter()
        .map(|s| s.text)
        .filter(|t| !t.trim().is_empty())
        .collect()
}

/// The scratch containers of one run on one engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchNames {
    pub token: String,
    /// The sync pair's source (a copy of `m8_src`).
    pub source: String,
    /// The sync pair's target (a copy of `m8_tgt`).
    pub target: String,
    /// Empty; restores land here.
    pub restore: String,
    /// Empty; transfers into this engine land here.
    pub transfer_target: String,
    /// PostgreSQL: a copy of the transfer source set `m8_xfer`.
    pub transfer_source: String,
}

impl ScratchNames {
    pub fn new(prefix: &str, token: &str) -> Self {
        let name = |role: &str| format!("{prefix}{token}_{role}");
        Self {
            token: token.to_owned(),
            source: name("src"),
            target: name("tgt"),
            restore: name("rst"),
            transfer_target: name("xt"),
            transfer_source: name("xs"),
        }
    }

    /// Every container, in drop order.
    pub fn all(&self) -> [&str; 5] {
        [
            &self.source,
            &self.target,
            &self.restore,
            &self.transfer_target,
            &self.transfer_source,
        ]
    }

    /// The `(canonical, scratch)` renames of the pair section.
    pub fn pair_renames(&self) -> [(&str, &str); 2] {
        [
            (CANONICAL_SOURCE, self.source.as_str()),
            (CANONICAL_TARGET, self.target.as_str()),
        ]
    }

    /// The rename of the transfer section.
    pub fn transfer_renames(&self) -> [(&str, &str); 1] {
        [(CANONICAL_TRANSFER_SOURCE, self.transfer_source.as_str())]
    }
}

/// `true` when `name` is a scratch container of `prefix` made more than `stale_after` seconds
/// before `now` (unix seconds). Containers without a readable token are never stale.
pub fn is_stale(name: &str, prefix: &str, now: u64, stale_after: u64) -> bool {
    roundtrip::scratch_token(name, prefix)
        .and_then(|t| roundtrip::token_age(t, now))
        .is_some_and(|age| age > stale_after)
}

/// Whether the engine's containers are schemas of one database (PostgreSQL, SQL Server) rather
/// than databases (MySQL, MariaDB). SQLite has one file per container.
pub fn uses_schemas(engine: Engine) -> bool {
    matches!(engine, Engine::Postgres | Engine::MsSql)
}

/// The container ref of `name` (`database`: the session database of schema engines; SQLite:
/// always `main` of its own file).
pub fn container(engine: Engine, database: &str, name: &str) -> ObjectRef {
    match engine {
        Engine::Sqlite => ObjectRef::database("main"),
        e if uses_schemas(e) => ObjectRef::schema(Some(database), name),
        _ => ObjectRef::database(name),
    }
}

/// `ident` quoted for `engine` (the harness's names and the seed's table names only: no quote
/// characters to double except PostgreSQL / SQLite `"`).
pub fn quote(engine: Engine, ident: &str) -> String {
    match engine {
        Engine::MySql | Engine::MariaDb => format!("`{}`", ident.replace('`', "``")),
        Engine::MsSql => format!("[{}]", ident.replace(']', "]]")),
        _ => format!("\"{}\"", ident.replace('"', "\"\"")),
    }
}

/// `table` in container `container` (SQLite: the session's own file, so the bare table).
pub fn qualified(engine: Engine, container: &str, table: &str) -> String {
    match engine {
        Engine::Sqlite => quote(engine, table),
        _ => format!("{}.{}", quote(engine, container), quote(engine, table)),
    }
}

/// Creates the empty container `name` (not for SQLite).
pub fn create_container(engine: Engine, name: &str) -> String {
    match engine {
        Engine::MySql | Engine::MariaDb => format!(
            "CREATE DATABASE {} DEFAULT CHARACTER SET utf8mb4",
            quote(engine, name)
        ),
        _ => format!("CREATE SCHEMA {}", quote(engine, name)),
    }
}

/// Drops the containers `names` and everything in them, if they exist (not for SQLite). SQL
/// Server cannot drop a schema that holds objects, so its batch drops foreign keys, views,
/// routines, tables and sequences first (triggers go with their tables).
pub fn drop_containers(engine: Engine, names: &[&str]) -> Vec<String> {
    match engine {
        Engine::Postgres => names
            .iter()
            .map(|n| format!("DROP SCHEMA IF EXISTS {} CASCADE", quote(engine, n)))
            .collect(),
        Engine::MySql | Engine::MariaDb => names
            .iter()
            .map(|n| format!("DROP DATABASE IF EXISTS {}", quote(engine, n)))
            .collect(),
        Engine::MsSql if !names.is_empty() => {
            let list = names
                .iter()
                .map(|n| format!("(N'{}')", n.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");
            vec![format!(
                "SET NOCOUNT ON;
DECLARE @schemas TABLE (name sysname PRIMARY KEY);
INSERT INTO @schemas (name) VALUES {list};
DECLARE @steps TABLE (seq int IDENTITY(1,1) PRIMARY KEY, stmt nvarchar(max) NOT NULL);
INSERT INTO @steps (stmt)
SELECT N'ALTER TABLE ' + QUOTENAME(s.name) + N'.' + QUOTENAME(t.name) + N' DROP CONSTRAINT '
       + QUOTENAME(fk.name)
FROM sys.foreign_keys fk
JOIN sys.tables t ON t.object_id = fk.parent_object_id
JOIN sys.schemas s ON s.schema_id = t.schema_id
WHERE s.name IN (SELECT name FROM @schemas);
INSERT INTO @steps (stmt)
SELECT N'DROP ' + CASE o.type WHEN 'V' THEN N'VIEW' WHEN 'P' THEN N'PROCEDURE'
                              WHEN 'U' THEN N'TABLE' WHEN 'SO' THEN N'SEQUENCE'
                              ELSE N'FUNCTION' END
       + N' ' + QUOTENAME(s.name) + N'.' + QUOTENAME(o.name)
FROM sys.objects o
JOIN sys.schemas s ON s.schema_id = o.schema_id
WHERE s.name IN (SELECT name FROM @schemas) AND o.type IN ('V', 'P', 'FN', 'IF', 'TF', 'U', 'SO')
ORDER BY CASE o.type WHEN 'V' THEN 1 WHEN 'P' THEN 2 WHEN 'U' THEN 4 WHEN 'SO' THEN 5 ELSE 3 END,
         o.name;
INSERT INTO @steps (stmt)
SELECT N'DROP SCHEMA ' + QUOTENAME(name) FROM @schemas WHERE SCHEMA_ID(name) IS NOT NULL;
DECLARE @sql nvarchar(max) = (SELECT STRING_AGG(stmt, N'; ') WITHIN GROUP (ORDER BY seq) FROM @steps);
IF @sql IS NOT NULL EXEC sys.sp_executesql @sql;"
            )]
        }
        _ => Vec::new(),
    }
}

/// Lists the containers whose name starts with `prefix` (one name per row, first column).
pub fn list_containers(engine: Engine, prefix: &str) -> Option<String> {
    let len = prefix.chars().count();
    let lit = prefix.replace('\'', "''");
    match engine {
        Engine::Postgres => Some(format!(
            "SELECT nspname::text FROM pg_catalog.pg_namespace WHERE left(nspname, {len}) = '{lit}'"
        )),
        Engine::MySql | Engine::MariaDb => Some(format!(
            "SELECT CAST(SCHEMA_NAME AS CHAR) FROM information_schema.SCHEMATA \
             WHERE LEFT(SCHEMA_NAME, {len}) = '{lit}'"
        )),
        Engine::MsSql => Some(format!(
            "SELECT CAST(name AS nvarchar(128)) FROM sys.schemas WHERE LEFT(name, {len}) = N'{lit}'"
        )),
        _ => None,
    }
}

/// The `[m8] structure_items` an engine must report: without its `skip_items`, and with the
/// routine's identity arguments where the engine has them (`function:order_total(integer)` on
/// PostgreSQL).
pub fn expected_items(
    items: &[String],
    skip: &[String],
    function: Option<&str>,
    signature: Option<&str>,
) -> Vec<String> {
    items
        .iter()
        .filter(|id| !skip.contains(id))
        .map(|id| match (function, signature) {
            (Some(f), Some(sig)) if id == &format!("function:{f}") => {
                format!("function:{f}({sig})")
            }
            _ => id.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::in_workspace;
    use pretty_assertions::assert_eq;

    #[test]
    fn sections_are_cut_at_their_markers() {
        let script = "a;\n-- m8:pair begin\nb;\nc;\n-- m8:pair end\n-- m8:xfer begin\nd;\n  -- m8:xfer end  \n";
        assert_eq!(section(script, "pair"), Some("b;\nc;\n"));
        assert_eq!(section(script, "xfer"), Some("d;\n"));
        assert_eq!(section(script, "other"), None);
        assert_eq!(section("-- m8:pair begin\nx;\n", "pair"), None);
    }

    #[test]
    fn renames_whole_words_only() {
        let text = "CREATE SCHEMA m8_src; m8_src.t, `m8_src`, N'm8_src', m8_src_x, xm8_src, \
                    m8_xfer_target, m8_xfer.m8_mood";
        let out = rename(text, &[("m8_src", "m8_t_a_src"), ("m8_xfer", "m8_t_a_xs")]);
        assert_eq!(
            out,
            "CREATE SCHEMA m8_t_a_src; m8_t_a_src.t, `m8_t_a_src`, N'm8_t_a_src', m8_src_x, \
             xm8_src, m8_xfer_target, m8_t_a_xs.m8_mood"
        );
        assert_eq!(rename_word("m8_src", "m8_src", "z"), "z");
        assert_eq!(rename_word("abc", "", "z"), "abc");
    }

    #[test]
    fn scratch_names_carry_a_token_and_age() {
        let token = roundtrip::token(1_000_000, 42);
        let names = ScratchNames::new(DEFAULT_PREFIX, &token);
        assert!(names.source.starts_with("m8_t_") && names.source.ends_with("_src"));
        assert_eq!(names.all().len(), 5);
        for n in names.all() {
            assert!(n.len() <= 63, "{n}");
            assert!(is_stale(n, DEFAULT_PREFIX, 1_000_000 + 7200, 3600), "{n}");
            assert!(!is_stale(n, DEFAULT_PREFIX, 1_000_000 + 60, 3600), "{n}");
        }
        assert!(!is_stale("m8_t_nottoken_src", DEFAULT_PREFIX, 9_999_999, 1));
        assert!(!is_stale("m8_src", DEFAULT_PREFIX, 9_999_999, 1));
    }

    #[test]
    fn engine_sql_helpers() {
        assert_eq!(
            qualified(Engine::Postgres, "s", "MixedCase"),
            "\"s\".\"MixedCase\""
        );
        assert_eq!(qualified(Engine::MySql, "d", "select"), "`d`.`select`");
        assert_eq!(qualified(Engine::MsSql, "s", "select"), "[s].[select]");
        assert_eq!(qualified(Engine::Sqlite, "main", "t"), "\"t\"");
        assert_eq!(
            container(Engine::Postgres, "oxbase_test", "x"),
            ObjectRef::schema(Some("oxbase_test"), "x")
        );
        assert_eq!(
            container(Engine::MariaDb, "oxbase_test", "x"),
            ObjectRef::database("x")
        );
        assert_eq!(
            container(Engine::Sqlite, "oxbase_test", "x"),
            ObjectRef::database("main")
        );
        assert_eq!(
            drop_containers(Engine::Postgres, &["a"]),
            vec!["DROP SCHEMA IF EXISTS \"a\" CASCADE".to_owned()]
        );
        let ms = drop_containers(Engine::MsSql, &["a", "b"]);
        assert_eq!(ms.len(), 1);
        assert!(ms[0].contains("(N'a'), (N'b')"));
        assert_eq!(drop_containers(Engine::MsSql, &[]), Vec::<String>::new());
        assert_eq!(
            drop_containers(Engine::Sqlite, &["a"]),
            Vec::<String>::new()
        );
        assert!(list_containers(Engine::MySql, "m8_t_").is_some());
        assert!(list_containers(Engine::Sqlite, "m8_t_").is_none());
    }

    #[test]
    fn expected_items_skip_and_sign() {
        let items: Vec<String> = ["table:a", "sequence:s", "function:f"]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(
            expected_items(
                &items,
                &["sequence:s".to_owned()],
                Some("f"),
                Some("integer")
            ),
            vec!["table:a".to_owned(), "function:f(integer)".to_owned()]
        );
        assert_eq!(expected_items(&items, &[], Some("f"), None), items);
    }

    /// Every networked seed has its sections, they split into statements, and the renamed text
    /// keeps no canonical container name (so a scratch copy cannot touch the canonical one).
    #[test]
    fn seed_scripts_have_sections_that_rename_cleanly() {
        let names = ScratchNames::new(DEFAULT_PREFIX, &roundtrip::token(1, 1));
        for (engine, file, sections) in [
            (
                Engine::Postgres,
                "tests/docker/init/postgres/06-m8.sql",
                &["pair", "xfer"][..],
            ),
            (
                Engine::MySql,
                "tests/docker/init/mysql/06-m8.sql",
                &["pair"],
            ),
            (
                Engine::MariaDb,
                "tests/docker/init/mariadb/06-m8.sql",
                &["pair"],
            ),
            (
                Engine::MsSql,
                "tests/docker/init/mssql/07-m8.sql",
                &["pair"],
            ),
        ] {
            let text = std::fs::read_to_string(in_workspace(file)).unwrap();
            assert!(text.is_ascii(), "{file} must stay ASCII");
            for &name in sections {
                let body = section(&text, name).unwrap_or_else(|| panic!("{file}: no {name}"));
                let renamed = if name == "xfer" {
                    rename(body, &names.transfer_renames())
                } else {
                    rename(body, &names.pair_renames())
                };
                for canonical in [
                    CANONICAL_SOURCE,
                    CANONICAL_TARGET,
                    CANONICAL_TRANSFER_SOURCE,
                ] {
                    assert_eq!(
                        rename_word(&renamed, canonical, "<hit>")
                            .matches("<hit>")
                            .count(),
                        0,
                        "{file} section {name} still names {canonical} after the rename"
                    );
                }
                let stmts = statements(engine, &renamed);
                assert!(
                    stmts.len() > 10,
                    "{file} {name}: {} statements",
                    stmts.len()
                );
            }
        }
        for file in [
            "tests/docker/init/sqlite/03-m8-src.sql",
            "tests/docker/init/sqlite/04-m8-tgt.sql",
        ] {
            let text = std::fs::read_to_string(in_workspace(file)).unwrap();
            assert!(text.is_ascii(), "{file} must stay ASCII");
            let stmts = statements(Engine::Sqlite, &text);
            assert!(stmts.len() > 10, "{file}: {} statements", stmts.len());
            // The trigger body is one statement.
            assert!(
                stmts
                    .iter()
                    .all(|s| !s.trim_start().starts_with("INSERT INTO order_audit")),
                "{file}: the trigger body was split"
            );
        }
    }
}
