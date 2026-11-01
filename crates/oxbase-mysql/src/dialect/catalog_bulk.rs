//! M6: the MySQL / MariaDB [`CatalogReader`] behind the schema cache
//! (`docs/m6-contracts.md` §3.2).
//!
//! The snapshot describes the session's current database (`DATABASE()`), which is the one
//! schema of the snapshot; every other database the login sees is listed in `databases`. A full
//! read is seven `information_schema` queries whatever the number of tables: settings,
//! databases, tables (with MariaDB sequences), columns (with key flags), foreign keys, routines
//! and routine parameters. `lower_case_table_names` decides the case rule of database and table
//! names (0: exact; 1 and 2: case-insensitive).
//!
//! A relation scope travels as one bound text of `"name"` entries (quotes doubled) that
//! `LOCATE` searches on lower-cased text, so it can only match more than asked for (another
//! case of the same name); the cache merges what comes back.

use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use oxbase_dialect::{CatalogReader, CatalogScope};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    CaseRules, CatalogColumn, CatalogForeignKey, CatalogRelation, CatalogRoutine, CatalogSchema,
    CatalogSnapshot, Error, IdentCase, RelationKind, Result, RoutineKind, Value,
};

use super::rows::{query, text};

const SETTINGS: &str = "SELECT DATABASE(), @@lower_case_table_names";

const DATABASES: &str = "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME";

/// The relation filter: `?` (1 = every table) and `?` (the name list).
macro_rules! name_filter {
    ($column:literal) => {
        concat!(
            "(? = 1 OR LOCATE(CONVERT(LOWER(CONCAT('\"', REPLACE(",
            $column,
            ", '\"', '\"\"'), '\"')) USING utf8mb4), CONVERT(LOWER(?) USING utf8mb4)) > 0)"
        )
    };
}

const TABLES: &str = concat!(
    "SELECT TABLE_NAME, TABLE_TYPE, TABLE_COMMENT FROM information_schema.TABLES \
     WHERE TABLE_SCHEMA = ? AND ",
    name_filter!("TABLE_NAME")
);

const COLUMNS: &str = concat!(
    "SELECT TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY \
     FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND ",
    name_filter!("TABLE_NAME"),
    " ORDER BY TABLE_NAME, ORDINAL_POSITION LIMIT ?"
);

const FOREIGN_KEYS: &str = concat!(
    "SELECT TABLE_NAME, CONSTRAINT_NAME, COLUMN_NAME, REFERENCED_TABLE_SCHEMA, \
            REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME \
     FROM information_schema.KEY_COLUMN_USAGE \
     WHERE TABLE_SCHEMA = ? AND REFERENCED_TABLE_NAME IS NOT NULL AND ",
    name_filter!("TABLE_NAME"),
    " ORDER BY TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION"
);

const ROUTINES: &str = "\
SELECT ROUTINE_NAME, ROUTINE_TYPE, DTD_IDENTIFIER FROM information_schema.ROUTINES \
WHERE ROUTINE_SCHEMA = ? AND ROUTINE_TYPE IN ('FUNCTION', 'PROCEDURE')";

const PARAMETERS: &str = "\
SELECT SPECIFIC_NAME, ROUTINE_TYPE, PARAMETER_MODE, PARAMETER_NAME, DTD_IDENTIFIER \
FROM information_schema.PARAMETERS \
WHERE SPECIFIC_SCHEMA = ? AND ORDINAL_POSITION > 0 \
ORDER BY SPECIFIC_NAME, ROUTINE_TYPE, ORDINAL_POSITION";

/// `"a""b"` entries, the list form `LOCATE` looks names up in.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The case rules for `lower_case_table_names`.
fn case_rules(lower_case_table_names: Option<i128>) -> CaseRules {
    let names = match lower_case_table_names {
        Some(1 | 2) => IdentCase::Insensitive,
        _ => IdentCase::Sensitive,
    };
    CaseRules {
        schemas: names,
        relations: names,
        columns: IdentCase::Insensitive,
        routines: IdentCase::Insensitive,
    }
}

/// What a scope asks of database `db`: `None` = nothing; `Some((all, list))` = every table
/// (`all`) or the tables of `list` (`"name"` entries), plus whether schema-level objects
/// (routines, sequences) are read.
fn scope_filter(scope: &CatalogScope, db: &str, rule: IdentCase) -> Option<(bool, String, bool)> {
    if scope.is_full() {
        return Some((true, String::new(), true));
    }
    let whole = scope
        .schemas
        .iter()
        .flatten()
        .any(|s| rule.matches(s, true, db));
    if whole {
        return Some((true, String::new(), true));
    }
    let list: String = scope
        .relations
        .iter()
        .flatten()
        .filter(|r| {
            r.schema
                .as_deref()
                .is_none_or(|s| rule.matches(s, true, db))
        })
        .map(|r| quoted(&r.name))
        .collect();
    (!list.is_empty()).then_some((false, list, false))
}

fn cut_comment(comment: Option<String>) -> Option<String> {
    comment.filter(|c| !c.is_empty()).map(|c| {
        if c.chars().count() > CatalogSnapshot::MAX_COMMENT_CHARS {
            c.chars().take(CatalogSnapshot::MAX_COMMENT_CHARS).collect()
        } else {
            c
        }
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The MySQL / MariaDB catalog reader. See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct MySqlCatalogReader;

#[async_trait]
impl CatalogReader for MySqlCatalogReader {
    async fn read(
        &self,
        c: &dyn Connection,
        scope: &CatalogScope,
        cancel: CancellationToken,
    ) -> Result<CatalogSnapshot> {
        let settings = query(c, SETTINGS, &[], &cancel).await?;
        let first = settings.first();
        let db = first.and_then(|r| r.text(0)).unwrap_or_default();
        let rules = case_rules(first.and_then(|r| r.int(1)));
        let mut snap = CatalogSnapshot::new(c.engine(), c.server_version().clone(), db.clone());
        snap.case_rules = rules;
        snap.databases = query(c, DATABASES, &[], &cancel)
            .await?
            .iter()
            .map(|r| r.string(0))
            .collect();
        if !db.is_empty() {
            snap.default_schema = Some(db.clone());
            snap.search_path = vec![db.clone()];
        }
        let filter = if db.is_empty() {
            None
        } else {
            scope_filter(scope, &db, rules.schemas)
        };
        let Some((all, list, schema_level)) = filter else {
            snap.read_at_ms = now_ms();
            return Ok(snap);
        };
        let filter_params = |extra: Option<Value>| {
            let mut p = vec![text(&db), Value::Int(i64::from(all)), text(&list)];
            p.extend(extra);
            p
        };

        let mut schema = CatalogSchema::new(db.as_str());
        let mut index: HashMap<String, usize> = HashMap::new();
        for row in query(c, TABLES, &filter_params(None), &cancel).await? {
            let name = row.string(0);
            let table_type = row.string(1).to_ascii_uppercase();
            let kind = match table_type.as_str() {
                "SEQUENCE" => {
                    if schema_level {
                        schema.sequences.push(name);
                    }
                    continue;
                }
                "VIEW" | "SYSTEM VIEW" => RelationKind::View,
                "TEMPORARY" | "LOCAL TEMPORARY" => RelationKind::Temporary,
                _ => RelationKind::Table,
            };
            let comment = if kind == RelationKind::View {
                None
            } else {
                cut_comment(row.text(2))
            };
            index.insert(name.clone(), schema.relations.len());
            schema.relations.push(CatalogRelation {
                name,
                kind,
                columns: Vec::new(),
                foreign_keys: Vec::new(),
                comment,
            });
        }

        let limit = i64::try_from(scope.max_columns.saturating_add(1)).unwrap_or(i64::MAX);
        let columns = query(c, COLUMNS, &filter_params(Some(Value::Int(limit))), &cancel).await?;
        for (read, row) in columns.into_iter().enumerate() {
            if read >= scope.max_columns {
                snap.truncated = true;
                break;
            }
            let Some(&i) = index.get(&row.string(0)) else {
                continue;
            };
            schema.relations[i].columns.push(CatalogColumn {
                name: row.string(1),
                native_type: row.string(2),
                nullable: row.flag(3),
                primary_key: row.string(4).eq_ignore_ascii_case("PRI"),
            });
        }

        let mut last: Option<(String, String)> = None;
        for row in query(c, FOREIGN_KEYS, &filter_params(None), &cancel).await? {
            let table = row.string(0);
            let constraint = row.string(1);
            let Some(&i) = index.get(&table) else {
                continue;
            };
            let rel = &mut schema.relations[i];
            let key = (table, constraint);
            if last.as_ref() == Some(&key)
                && let Some(fk) = rel.foreign_keys.last_mut()
            {
                fk.columns.push(row.string(2));
                fk.ref_columns.push(row.string(5));
                continue;
            }
            last = Some(key);
            let ref_schema = row.string(3);
            rel.foreign_keys.push(CatalogForeignKey {
                columns: vec![row.string(2)],
                ref_schema: (ref_schema != db).then_some(ref_schema),
                ref_table: row.string(4),
                ref_columns: vec![row.string(5)],
            });
        }

        if schema_level {
            let mut args: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
            for row in query(c, PARAMETERS, &[text(&db)], &cancel).await? {
                let key = (row.string(0), row.string(1).to_ascii_uppercase());
                let mode = row.non_empty(2);
                let name = row.string(3);
                let ty = row.string(4);
                let arg = match mode {
                    Some(mode) if key.1 == "PROCEDURE" => format!("{mode} {name} {ty}"),
                    _ => format!("{name} {ty}"),
                };
                args.entry(key).or_default().push(arg);
            }
            for row in query(c, ROUTINES, &[text(&db)], &cancel).await? {
                let name = row.string(0);
                let routine_type = row.string(1).to_ascii_uppercase();
                let procedure = routine_type == "PROCEDURE";
                let routine_args = args
                    .remove(&(name.clone(), routine_type))
                    .unwrap_or_default()
                    .join(", ");
                schema.routines.push(CatalogRoutine {
                    name,
                    kind: if procedure {
                        RoutineKind::Procedure
                    } else {
                        RoutineKind::Function
                    },
                    args: routine_args,
                    returns: if procedure { None } else { row.non_empty(2) },
                });
            }
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        snap.schemas = vec![schema];
        snap.read_at_ms = now_ms();
        snap.normalize();
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_dialect::RelationName;

    #[test]
    fn case_rules_follow_lower_case_table_names() {
        assert_eq!(case_rules(Some(0)).relations, IdentCase::Sensitive);
        assert_eq!(case_rules(Some(1)).relations, IdentCase::Insensitive);
        assert_eq!(case_rules(Some(2)).schemas, IdentCase::Insensitive);
        assert_eq!(case_rules(None).columns, IdentCase::Insensitive);
    }

    #[test]
    fn scopes_filter_the_current_database() {
        let rule = IdentCase::Sensitive;
        assert_eq!(
            scope_filter(&CatalogScope::full(), "app", rule),
            Some((true, String::new(), true))
        );
        let schemas = CatalogScope {
            schemas: Some(vec!["app".into()]),
            ..CatalogScope::full()
        };
        assert_eq!(
            scope_filter(&schemas, "app", rule),
            Some((true, String::new(), true))
        );
        let other = CatalogScope {
            schemas: Some(vec!["App".into()]),
            ..CatalogScope::full()
        };
        assert_eq!(scope_filter(&other, "app", rule), None);
        assert!(scope_filter(&other, "app", IdentCase::Insensitive).is_some());
        let relations = CatalogScope {
            relations: Some(vec![
                RelationName {
                    schema: None,
                    name: "a\"b".into(),
                },
                RelationName {
                    schema: Some("other".into()),
                    name: "x".into(),
                },
                RelationName {
                    schema: Some("app".into()),
                    name: "c".into(),
                },
            ]),
            ..CatalogScope::full()
        };
        assert_eq!(
            scope_filter(&relations, "app", rule),
            Some((false, "\"a\"\"b\"\"c\"".into(), false))
        );
    }

    #[test]
    fn queries_bind_the_scope() {
        for sql in [TABLES, COLUMNS, FOREIGN_KEYS] {
            assert_eq!(
                sql.matches('?').count(),
                3 + usize::from(sql == COLUMNS),
                "{sql}"
            );
        }
    }
}
