//! Built-in snippets (`docs/m6-contracts.md` §6), written for Oxbase. User snippets live in the
//! app database (`oxbase_store::SnippetRepository`) and are listed together with these by
//! `oxbase_services::snippets::SnippetService`.

use oxbase_types::{Engine, Snippet};

/// (slug, name, prefix, body, description, engines; empty = all SQL engines)
type Row = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static [Engine],
);

const PG: &[Engine] = &[Engine::Postgres];
const PG_SQLITE: &[Engine] = &[Engine::Postgres, Engine::Sqlite];
const MYSQL: &[Engine] = &[Engine::MySql, Engine::MariaDb];
const MSSQL: &[Engine] = &[Engine::MsSql];
const LIMIT_ENGINES: &[Engine] = &[
    Engine::Postgres,
    Engine::MySql,
    Engine::MariaDb,
    Engine::Sqlite,
];
const ALL: &[Engine] = &[];

const ROWS: &[Row] = &[
    (
        "select",
        "Select rows",
        "sel",
        "SELECT ${1:*}\nFROM ${2:table_name}\nWHERE ${3:condition};$0",
        "Rows of one table that match a condition.",
        ALL,
    ),
    (
        "select-limit",
        "Select first rows",
        "sell",
        "SELECT ${1:*}\nFROM ${2:table_name}\nORDER BY ${3:column_name}\nLIMIT ${4:100};$0",
        "The first rows of a table in a given order.",
        LIMIT_ENGINES,
    ),
    (
        "select-top",
        "Select first rows",
        "sell",
        "SELECT TOP (${4:100}) ${1:*}\nFROM ${2:table_name}\nORDER BY ${3:column_name};$0",
        "The first rows of a table in a given order.",
        MSSQL,
    ),
    (
        "count-group",
        "Count per group",
        "selc",
        "SELECT ${1:column_name}, COUNT(*) AS ${2:row_count}\nFROM ${3:table_name}\nGROUP BY ${1:column_name}\nORDER BY ${2:row_count} DESC;$0",
        "How many rows each value of a column has.",
        ALL,
    ),
    (
        "insert",
        "Insert a row",
        "ins",
        "INSERT INTO ${1:table_name} (${2:column_names})\nVALUES (${3:values});$0",
        "Adds one row.",
        ALL,
    ),
    (
        "update",
        "Update rows",
        "upd",
        "UPDATE ${1:table_name}\nSET ${2:column_name} = ${3:value}\nWHERE ${4:condition};$0",
        "Changes the rows that match a condition.",
        ALL,
    ),
    (
        "delete",
        "Delete rows",
        "del",
        "DELETE FROM ${1:table_name}\nWHERE ${2:condition};$0",
        "Removes the rows that match a condition.",
        ALL,
    ),
    (
        "inner-join",
        "Inner join",
        "join",
        "INNER JOIN ${1:table_name} ${2:alias} ON ${2:alias}.${3:column_name} = ${4:other}.${5:column_name}$0",
        "Rows that have a partner in another table.",
        ALL,
    ),
    (
        "left-join",
        "Left join",
        "ljoin",
        "LEFT JOIN ${1:table_name} ${2:alias} ON ${2:alias}.${3:column_name} = ${4:other}.${5:column_name}$0",
        "Every row, with its partner in another table when there is one.",
        ALL,
    ),
    (
        "cte",
        "Common table expression",
        "cte",
        "WITH ${1:name} AS (\n    ${2:SELECT 1}\n)\nSELECT ${3:*}\nFROM ${1:name};$0",
        "A named subquery used by the query that follows.",
        ALL,
    ),
    (
        "case",
        "Case expression",
        "case",
        "CASE\n    WHEN ${1:condition} THEN ${2:result}\n    ELSE ${3:other_result}\nEND$0",
        "A value that depends on conditions.",
        ALL,
    ),
    (
        "exists",
        "Exists subquery",
        "exists",
        "EXISTS (\n    SELECT 1\n    FROM ${1:table_name}\n    WHERE ${2:condition}\n)$0",
        "True when a subquery returns at least one row.",
        ALL,
    ),
    (
        "create-table",
        "Create table",
        "ctab",
        "CREATE TABLE ${1:table_name} (\n    ${2:id} ${3:integer} PRIMARY KEY,\n    ${4:column_name} ${5:type}$0\n);",
        "A new table with a primary key.",
        ALL,
    ),
    (
        "create-index",
        "Create index",
        "cidx",
        "CREATE INDEX ${1:index_name} ON ${2:table_name} (${3:column_name});$0",
        "An index on one or more columns.",
        ALL,
    ),
    (
        "upsert-on-conflict",
        "Insert or update",
        "upsert",
        "INSERT INTO ${1:table_name} (${2:key_column}, ${3:column_name})\nVALUES (${4:key}, ${5:value})\nON CONFLICT (${2:key_column}) DO UPDATE SET ${3:column_name} = excluded.${3:column_name};$0",
        "Adds a row, or updates the row that has the same key.",
        PG_SQLITE,
    ),
    (
        "upsert-duplicate-key",
        "Insert or update",
        "upsert",
        "INSERT INTO ${1:table_name} (${2:key_column}, ${3:column_name})\nVALUES (${4:key}, ${5:value})\nON DUPLICATE KEY UPDATE ${3:column_name} = VALUES(${3:column_name});$0",
        "Adds a row, or updates the row that has the same unique key.",
        MYSQL,
    ),
    (
        "transaction",
        "Transaction",
        "tran",
        "BEGIN;\n${1:-- statements}\nCOMMIT;$0",
        "Statements that succeed or fail together.",
        LIMIT_ENGINES,
    ),
    (
        "transaction-tsql",
        "Transaction",
        "tran",
        "BEGIN TRANSACTION;\n${1:-- statements}\nCOMMIT TRANSACTION;$0",
        "Statements that succeed or fail together.",
        MSSQL,
    ),
    (
        "do-block",
        "Anonymous code block",
        "do",
        "DO \\$\\$\nBEGIN\n    ${1:-- statements}\nEND\n\\$\\$;$0",
        "Runs procedural code once without creating a function.",
        PG,
    ),
];

/// Every built-in snippet (ids `builtin:<slug>`), in a fixed order.
pub fn builtin_snippets() -> Vec<Snippet> {
    ROWS.iter()
        .map(|(slug, name, prefix, body, description, engines)| Snippet {
            id: format!("builtin:{slug}"),
            name: (*name).to_owned(),
            prefix: (*prefix).to_owned(),
            body: (*body).to_owned(),
            description: (*description).to_owned(),
            engines: engines.to_vec(),
            builtin: true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use oxbase_types::{SnippetInput, validate_snippet_body};

    use super::*;

    #[test]
    fn builtins_are_valid_unique_and_complete_per_engine() {
        let all = builtin_snippets();
        let mut ids = HashSet::new();
        for s in &all {
            assert!(ids.insert(s.id.clone()), "duplicate id {}", s.id);
            validate_snippet_body(&s.body).unwrap_or_else(|e| panic!("{}: {e}", s.id));
            SnippetInput {
                id: None,
                name: s.name.clone(),
                prefix: s.prefix.clone(),
                body: s.body.clone(),
                description: s.description.clone(),
                engines: s.engines.clone(),
            }
            .validate()
            .unwrap_or_else(|e| panic!("{}: {e}", s.id));
            assert!(s.builtin);
        }
        // On every SQL engine each prefix is offered at most once.
        for engine in [
            Engine::Postgres,
            Engine::MySql,
            Engine::MariaDb,
            Engine::Sqlite,
            Engine::MsSql,
        ] {
            let mut prefixes = HashSet::new();
            for s in all.iter().filter(|s| s.applies_to(engine)) {
                assert!(prefixes.insert(&s.prefix), "{engine}: {} twice", s.prefix);
            }
            assert!(prefixes.contains(&"sell".to_owned()), "{engine}");
            assert!(prefixes.contains(&"tran".to_owned()), "{engine}");
        }
    }
}
