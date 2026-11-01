//! M6: the SQLite [`CatalogReader`] behind the schema cache (`docs/m6-contracts.md` §3.2).
//!
//! The snapshot holds one schema per database of the session (`main`, each attached database,
//! and `temp` when it holds objects); `databases` lists the attached ones. Per database, three
//! queries whatever the number of tables: its tables and views from the catalog table, every
//! column through `pragma_table_xinfo` joined to it, and every foreign key through
//! `pragma_foreign_key_list` joined to it (table-valued pragma functions, SQLite 3.16+). SQLite
//! has no stored routines, sequences or user types.
//!
//! The only name written into SQL text is a database name the session itself reported in
//! `pragma_database_list`, quoted as an identifier (the rule of `catalog.rs`); table names and
//! the relation scope are bound. Virtual tables are listed without columns (their module may
//! not be loaded in this connection), and when a broken view makes the column query fail, it is
//! run again for tables only and views stay without columns.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use oxbase_dialect::{AnsiQuoting, CatalogReader, CatalogScope, Quoting};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    CatalogColumn, CatalogForeignKey, CatalogRelation, CatalogSchema, CatalogSnapshot, Engine,
    RelationKind, Result, ServerVersion, Value,
};

use super::catalog::{Catalog, check, schema_table_for};

/// Excludes SQLite's own tables (`sqlite_sequence`, `sqlite_stat1`, ...).
const NOT_INTERNAL: &str = "m.name NOT LIKE 'sqlite\\_%' ESCAPE '\\'";

/// The relation filter: `?1` (1 = every table) or `?2` (the `"name"` list).
const FILTER: &str =
    "(?1 = 1 OR instr(lower(?2), lower('\"' || replace(m.name, '\"', '\"\"') || '\"')) > 0)";

/// `"a""b"` entries, the list form `instr` looks names up in.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The pragma that lists columns: `pragma_table_xinfo` from 3.26 (generated columns),
/// `pragma_table_info` before.
fn column_pragma(v: &ServerVersion) -> &'static str {
    if v.at_least(3, 26) {
        "pragma_table_xinfo"
    } else {
        "pragma_table_info"
    }
}

/// What a scope asks of one database: `None` = nothing; `Some(None)` = everything;
/// `Some(Some(list))` = the relations of `list`.
fn scope_for(scope: &CatalogScope, database: &str) -> Option<Option<String>> {
    if scope.is_full()
        || scope
            .schemas
            .iter()
            .flatten()
            .any(|s| s.eq_ignore_ascii_case(database))
    {
        return Some(None);
    }
    let list: String = scope
        .relations
        .iter()
        .flatten()
        .filter(|r| {
            r.schema
                .as_deref()
                .unwrap_or("main")
                .eq_ignore_ascii_case(database)
        })
        .map(|r| quoted(&r.name))
        .collect();
    (!list.is_empty()).then_some(Some(list))
}

/// The SQLite catalog reader. See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteCatalogReader;

#[async_trait]
impl CatalogReader for SqliteCatalogReader {
    async fn read(
        &self,
        c: &dyn Connection,
        scope: &CatalogScope,
        cancel: CancellationToken,
    ) -> Result<CatalogSnapshot> {
        let cat = Catalog::new(c, &cancel);
        let version = c.server_version().clone();
        let table = schema_table_for(&version);
        let pragma = column_pragma(&version);
        let databases = cat.databases().await?;
        let mut snap = CatalogSnapshot::new(Engine::Sqlite, version, "main");
        snap.default_schema = Some("main".to_owned());
        snap.databases = databases
            .iter()
            .map(|d| d.name.clone())
            .filter(|n| !n.eq_ignore_ascii_case("main") && !n.eq_ignore_ascii_case("temp"))
            .collect();
        // Unqualified names: temp first, then main, then attached databases in attach order.
        let mut path: Vec<String> = databases
            .iter()
            .filter(|d| d.name.eq_ignore_ascii_case("temp"))
            .map(|d| d.name.clone())
            .collect();
        path.extend(
            databases
                .iter()
                .filter(|d| !d.name.eq_ignore_ascii_case("temp"))
                .map(|d| d.name.clone()),
        );
        snap.search_path = path;

        let mut columns_read = 0_usize;
        for database in &databases {
            let Some(filter) = scope_for(scope, &database.name) else {
                continue;
            };
            let from = format!("{}.{table}", AnsiQuoting.quote_ident(&database.name));
            let (all, list) = match filter {
                None => (1, String::new()),
                Some(list) => (0, list),
            };
            let params = [Value::Int(all), Value::Text(list.clone())];
            let mut schema = CatalogSchema::new(database.name.as_str());
            let mut index: HashMap<String, usize> = HashMap::new();
            let relations_sql = format!(
                "SELECT m.name, m.type, m.sql FROM {from} m \
                 WHERE m.type IN ('table', 'view') AND {NOT_INTERNAL} AND {FILTER}"
            );
            for row in cat.rows(&relations_sql, &params).await? {
                let name = row.string(0);
                let kind = if row.string(1).eq_ignore_ascii_case("view") {
                    RelationKind::View
                } else if database.name.eq_ignore_ascii_case("temp") {
                    RelationKind::Temporary
                } else {
                    RelationKind::Table
                };
                index.insert(name.to_ascii_lowercase(), schema.relations.len());
                schema.relations.push(CatalogRelation {
                    name,
                    kind,
                    columns: Vec::new(),
                    foreign_keys: Vec::new(),
                    comment: None,
                });
            }

            let remaining = scope.max_columns.saturating_sub(columns_read);
            let limit = i64::try_from(remaining.saturating_add(1)).unwrap_or(i64::MAX);
            let column_params = [
                Value::Int(all),
                Value::Text(list.clone()),
                Value::Text(database.name.clone()),
                Value::Int(limit),
            ];
            let columns_sql = |kinds: &str| {
                format!(
                    "SELECT m.name, p.name, p.type, p.\"notnull\", p.pk \
                     FROM {from} m JOIN {pragma}(m.name, ?3) p \
                     WHERE m.type IN ({kinds}) AND {NOT_INTERNAL} AND {FILTER} \
                       AND (m.sql IS NULL OR upper(m.sql) NOT LIKE 'CREATE VIRTUAL%') \
                     ORDER BY m.name, p.cid LIMIT ?4"
                )
            };
            let rows = match cat
                .rows(&columns_sql("'table', 'view'"), &column_params)
                .await
            {
                Ok(rows) => rows,
                Err(e) if e.is_cancelled() => return Err(e),
                // A view over a dropped table fails the pragma: read tables only.
                Err(_) => cat.rows(&columns_sql("'table'"), &column_params).await?,
            };
            let mut read = 0_usize;
            for row in rows {
                if read >= remaining {
                    snap.truncated = true;
                    break;
                }
                read += 1;
                let Some(&i) = index.get(&row.string(0).to_ascii_lowercase()) else {
                    continue;
                };
                schema.relations[i].columns.push(CatalogColumn {
                    name: row.string(1),
                    native_type: row.string(2),
                    nullable: !row.flag(3),
                    primary_key: row.int(4).is_some_and(|pk| pk > 0),
                });
            }
            columns_read += read;

            let fk_sql = format!(
                "SELECT m.name, f.id, f.\"table\", f.\"from\", f.\"to\" \
                 FROM {from} m JOIN pragma_foreign_key_list(m.name, ?3) f \
                 WHERE m.type = 'table' AND {NOT_INTERNAL} AND {FILTER} \
                 ORDER BY m.name, f.id, f.seq"
            );
            let fk_params = [
                Value::Int(all),
                Value::Text(list),
                Value::Text(database.name.clone()),
            ];
            let mut last: Option<(String, i64)> = None;
            let mut implicit: Vec<(usize, usize)> = Vec::new();
            for row in cat.rows(&fk_sql, &fk_params).await? {
                let table_name = row.string(0);
                let Some(&i) = index.get(&table_name.to_ascii_lowercase()) else {
                    continue;
                };
                let id = row.int(1).unwrap_or_default();
                let to = row.text(4);
                let rel = &mut schema.relations[i];
                let key = (table_name, id);
                if last.as_ref() == Some(&key)
                    && let Some(fk) = rel.foreign_keys.last_mut()
                {
                    fk.columns.push(row.string(3));
                    fk.ref_columns.extend(to);
                    continue;
                }
                last = Some(key);
                if to.is_none() {
                    implicit.push((i, rel.foreign_keys.len()));
                }
                rel.foreign_keys.push(CatalogForeignKey {
                    columns: vec![row.string(3)],
                    ref_schema: None,
                    ref_table: row.string(2),
                    ref_columns: to.into_iter().collect(),
                });
            }
            // `REFERENCES parent` without columns means the parent's primary key.
            for (rel, fk) in implicit {
                let parent = schema.relations[rel].foreign_keys[fk]
                    .ref_table
                    .to_ascii_lowercase();
                let pk: Vec<String> = index
                    .get(&parent)
                    .map(|&p| {
                        schema.relations[p]
                            .columns
                            .iter()
                            .filter(|c| c.primary_key)
                            .map(|c| c.name.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                schema.relations[rel].foreign_keys[fk].ref_columns = pk;
            }
            snap.schemas.push(schema);
        }
        check(&cancel)?;
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
    fn scopes_pick_databases_and_relations() {
        assert_eq!(scope_for(&CatalogScope::full(), "main"), Some(None));
        let schemas = CatalogScope {
            schemas: Some(vec!["AUX".into()]),
            ..CatalogScope::full()
        };
        assert_eq!(scope_for(&schemas, "aux"), Some(None));
        assert_eq!(scope_for(&schemas, "main"), None);
        let relations = CatalogScope {
            relations: Some(vec![
                RelationName {
                    schema: None,
                    name: "t".into(),
                },
                RelationName {
                    schema: Some("aux".into()),
                    name: "u\"v".into(),
                },
            ]),
            ..CatalogScope::full()
        };
        assert_eq!(scope_for(&relations, "main"), Some(Some("\"t\"".into())));
        assert_eq!(
            scope_for(&relations, "aux"),
            Some(Some("\"u\"\"v\"".into()))
        );
        assert_eq!(scope_for(&relations, "temp"), None);
    }

    #[test]
    fn column_pragma_by_version() {
        assert_eq!(
            column_pragma(&ServerVersion::new(3, 25, 0)),
            "pragma_table_info"
        );
        assert_eq!(
            column_pragma(&ServerVersion::new(3, 50, 0)),
            "pragma_table_xinfo"
        );
    }
}
