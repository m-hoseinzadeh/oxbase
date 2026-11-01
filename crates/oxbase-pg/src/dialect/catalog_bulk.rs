//! M6: the PostgreSQL [`CatalogReader`] behind the schema cache (`docs/m6-contracts.md` §3.2).
//!
//! A full read is eight catalog queries whatever the number of tables: settings, search path,
//! databases, schemas, relations (tables, views, materialized views, foreign tables and
//! sequences), columns (with primary-key flags), foreign keys, routines and types. Every query
//! is fixed text; the scope travels as bound parameters:
//!
//! - `$1` (`bool`): include system schemas;
//! - `$2` (`text`): the scope's schemas as a list of `"name"` entries (quotes doubled), or NULL;
//! - `$3` (`text`): the scope's relations as `"schema"."name"` entries, or NULL;
//! - `$4` (`int8`): the column limit plus one (the extra row tells the reader to truncate).
//!
//! With both lists NULL the read is a full one. A list is matched with `strpos` on lower-cased
//! text, which can only ever match *more* than asked for (another case of the same name): the
//! cache merges what comes back, so an extra relation is simply refreshed too.

use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use oxbase_dialect::{CatalogReader, CatalogScope};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    CatalogColumn, CatalogForeignKey, CatalogRelation, CatalogRoutine, CatalogSchema,
    CatalogSnapshot, Engine, Error, RelationKind, Result, RoutineKind, ServerVersion, Value,
};

use super::rows::query;
use super::sql::at_least;

/// The scope filter on a schema-level object in namespace `n`.
macro_rules! schema_filter {
    () => {
        "(($2::text IS NULL AND $3::text IS NULL) \
           OR strpos(lower($2::text), lower('\"' || replace(n.nspname, '\"', '\"\"') || '\"')) > 0) \
         AND n.nspname !~ '^pg_(toast_)?temp_' AND n.nspname !~ '^pg_toast' \
         AND ($1::bool OR n.nspname NOT IN ('pg_catalog', 'information_schema') \
              OR strpos(lower(coalesce($2::text, '')), \
                        lower('\"' || replace(n.nspname, '\"', '\"\"') || '\"')) > 0)"
    };
}

/// The scope filter on relation `c` in namespace `n`.
macro_rules! relation_filter {
    () => {
        "(($2::text IS NULL AND $3::text IS NULL) \
           OR strpos(lower($2::text), lower('\"' || replace(n.nspname, '\"', '\"\"') || '\"')) > 0 \
           OR strpos(lower($3::text), lower('\"' || replace(n.nspname, '\"', '\"\"') || '\".\"' \
                     || replace(c.relname, '\"', '\"\"') || '\"')) > 0) \
         AND n.nspname !~ '^pg_(toast_)?temp_' AND n.nspname !~ '^pg_toast' \
         AND ($1::bool OR n.nspname NOT IN ('pg_catalog', 'information_schema') \
              OR strpos(lower(coalesce($2::text, '')), \
                        lower('\"' || replace(n.nspname, '\"', '\"\"') || '\"')) > 0 \
              OR strpos(lower(coalesce($3::text, '')), \
                        lower('\"' || replace(n.nspname, '\"', '\"\"') || '\".')) > 0)"
    };
}

const SETTINGS: &str = "\
SELECT pg_catalog.current_database()::text AS database, \
       pg_catalog.current_schema()::text AS default_schema";

const SEARCH_PATH: &str = "\
SELECT u.s::text AS name \
FROM pg_catalog.unnest(pg_catalog.current_schemas(false)) WITH ORDINALITY AS u(s, ord) \
ORDER BY u.ord";

const DATABASES: &str = "\
SELECT d.datname::text AS name \
FROM pg_catalog.pg_database d \
WHERE NOT d.datistemplate AND d.datallowconn \
  AND pg_catalog.has_database_privilege(d.oid, 'CONNECT') \
ORDER BY 1";

const SCHEMAS: &str = concat!(
    "SELECT n.nspname::text AS name, \
            (n.nspname IN ('pg_catalog', 'information_schema')) AS is_system \
     FROM pg_catalog.pg_namespace n WHERE ",
    schema_filter!()
);

const RELATIONS: &str = concat!(
    "SELECT n.nspname::text AS schema_name, c.relname::text AS name, c.relkind::text AS relkind, \
            c.relpersistence::text AS persistence, d.description AS comment \
     FROM pg_catalog.pg_class c \
     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
     LEFT JOIN pg_catalog.pg_description d ON d.objoid = c.oid \
          AND d.classoid = 'pg_catalog.pg_class'::pg_catalog.regclass AND d.objsubid = 0 \
     WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S') AND ",
    relation_filter!()
);

const COLUMNS: &str = concat!(
    "SELECT n.nspname::text AS schema_name, c.relname::text AS rel, a.attname::text AS name, \
            pg_catalog.format_type(a.atttypid, a.atttypmod) AS native_type, \
            (NOT a.attnotnull) AS nullable, \
            coalesce(a.attnum = ANY (pk.conkey), false) AS primary_key \
     FROM pg_catalog.pg_attribute a \
     JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
     LEFT JOIN pg_catalog.pg_constraint pk ON pk.conrelid = c.oid AND pk.contype = 'p' \
     WHERE a.attnum > 0 AND NOT a.attisdropped AND c.relkind IN ('r', 'p', 'v', 'm', 'f') AND ",
    relation_filter!(),
    " ORDER BY c.oid, a.attnum LIMIT $4::int8"
);

/// Multi-argument `unnest` is grammar (`ROWS FROM`), not a function: it cannot be
/// schema-qualified.
const FOREIGN_KEYS: &str = concat!(
    "SELECT n.nspname::text AS schema_name, c.relname::text AS rel, con.oid::int8 AS id, \
            rn.nspname::text AS ref_schema, rc.relname::text AS ref_table, \
            a.attname::text AS col, ra.attname::text AS ref_col \
     FROM pg_catalog.pg_constraint con \
     JOIN pg_catalog.pg_class c ON c.oid = con.conrelid \
     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
     JOIN pg_catalog.pg_class rc ON rc.oid = con.confrelid \
     JOIN pg_catalog.pg_namespace rn ON rn.oid = rc.relnamespace \
     CROSS JOIN LATERAL unnest(con.conkey, con.confkey) WITH ORDINALITY AS k(attnum, refnum, ord) \
     JOIN pg_catalog.pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum \
     JOIN pg_catalog.pg_attribute ra ON ra.attrelid = con.confrelid AND ra.attnum = k.refnum \
     WHERE con.contype = 'f' AND ",
    relation_filter!(),
    " ORDER BY con.oid, k.ord"
);

/// Routines, PostgreSQL 11+ (`prokind`).
const ROUTINES: &str = concat!(
    "SELECT n.nspname::text AS schema_name, p.proname::text AS name, p.prokind::text AS kind, \
            pg_catalog.pg_get_function_identity_arguments(p.oid) AS args, \
            pg_catalog.pg_get_function_result(p.oid) AS returns \
     FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
     WHERE ",
    schema_filter!()
);

/// Routines before PostgreSQL 11 (`proisagg`, no procedures).
const ROUTINES_10: &str = concat!(
    "SELECT n.nspname::text AS schema_name, p.proname::text AS name, \
            CASE WHEN p.proisagg THEN 'a' WHEN p.proiswindow THEN 'w' ELSE 'f' END AS kind, \
            pg_catalog.pg_get_function_identity_arguments(p.oid) AS args, \
            pg_catalog.pg_get_function_result(p.oid) AS returns \
     FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
     WHERE ",
    schema_filter!()
);

/// Enums, domains, ranges, multiranges and stand-alone composite types.
const TYPES: &str = concat!(
    "SELECT n.nspname::text AS schema_name, t.typname::text AS name \
     FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
     WHERE (t.typtype IN ('e', 'd', 'r', 'm') \
            OR (t.typtype = 'c' AND EXISTS (SELECT 1 FROM pg_catalog.pg_class tc \
                                            WHERE tc.oid = t.typrelid AND tc.relkind = 'c'))) \
       AND ",
    schema_filter!()
);

/// The routine query for `v`.
fn routines_sql(v: &ServerVersion) -> &'static str {
    if at_least(v, 11) {
        ROUTINES
    } else {
        ROUTINES_10
    }
}

/// `"a""b"` entries, the list form `strpos` looks names up in.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A `"schema"."name"` entry.
fn qualified(schema: &str, name: &str) -> String {
    let mut out = quoted(schema);
    out.push('.');
    out.push_str(&quoted(name));
    out
}

/// The bound `$1`-`$3` of a scope; `default_schema` stands in for relations without a schema.
fn scope_params(scope: &CatalogScope, default_schema: &str) -> [Value; 3] {
    let schemas = scope.schemas.as_ref().map_or(Value::Null, |s| {
        Value::Text(s.iter().map(|n| quoted(n)).collect())
    });
    let relations = scope.relations.as_ref().map_or(Value::Null, |r| {
        Value::Text(
            r.iter()
                .map(|r| {
                    let schema = r.schema.as_deref().unwrap_or(default_schema);
                    qualified(schema, &r.name)
                })
                .collect(),
        )
    });
    [Value::Bool(scope.include_system), schemas, relations]
}

/// The comment cut to [`CatalogSnapshot::MAX_COMMENT_CHARS`] characters.
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

/// Gathers the rows of the catalog queries into schemas.
#[derive(Default)]
struct Builder {
    schemas: BTreeMap<String, CatalogSchema>,
    /// (schema, relation) → index in that schema's `relations`.
    relations: HashMap<(String, String), usize>,
}

impl Builder {
    fn schema(&mut self, name: &str) -> &mut CatalogSchema {
        self.schemas
            .entry(name.to_owned())
            .or_insert_with(|| CatalogSchema::new(name))
    }

    fn relation(
        &mut self,
        schema: &str,
        name: String,
        kind: RelationKind,
        comment: Option<String>,
    ) {
        let s = self.schema(schema);
        s.relations.push(CatalogRelation {
            name: name.clone(),
            kind,
            columns: Vec::new(),
            foreign_keys: Vec::new(),
            comment,
        });
        let index = s.relations.len() - 1;
        self.relations.insert((schema.to_owned(), name), index);
    }

    fn relation_mut(&mut self, schema: &str, name: &str) -> Option<&mut CatalogRelation> {
        let index = *self.relations.get(&(schema.to_owned(), name.to_owned()))?;
        self.schemas.get_mut(schema)?.relations.get_mut(index)
    }
}

/// The PostgreSQL catalog reader. See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct PgCatalogReader;

#[async_trait]
impl CatalogReader for PgCatalogReader {
    async fn read(
        &self,
        c: &dyn Connection,
        scope: &CatalogScope,
        cancel: CancellationToken,
    ) -> Result<CatalogSnapshot> {
        let version = c.server_version().clone();
        let settings = query(c, SETTINGS, &[], &cancel).await?;
        let first = settings.first();
        let database = match first {
            Some(row) => row.string("database")?,
            None => c.current_database().unwrap_or_default(),
        };
        let default_schema = match first {
            Some(row) => row.opt_string("default_schema")?,
            None => None,
        };
        let mut snap = CatalogSnapshot::new(Engine::Postgres, version.clone(), database);
        snap.search_path = query(c, SEARCH_PATH, &[], &cancel)
            .await?
            .iter()
            .map(|r| r.string("name"))
            .collect::<Result<_>>()?;
        snap.default_schema = default_schema
            .clone()
            .or_else(|| snap.search_path.first().cloned());
        snap.databases = query(c, DATABASES, &[], &cancel)
            .await?
            .iter()
            .map(|r| r.string("name"))
            .collect::<Result<_>>()?;

        let fallback = snap
            .default_schema
            .clone()
            .unwrap_or_else(|| "public".into());
        let [system, schemas, relations] = scope_params(scope, &fallback);
        let schema_params = [system.clone(), schemas.clone(), relations.clone()];
        let relations_only = scope.schemas.is_none() && scope.relations.is_some();
        let mut b = Builder::default();

        if !relations_only {
            for row in query(c, SCHEMAS, &schema_params, &cancel).await?.iter() {
                let name = row.string("name")?;
                b.schema(&name).system = row.flag("is_system")?;
            }
        }
        for row in query(c, RELATIONS, &schema_params, &cancel).await?.iter() {
            let schema = row.string("schema_name")?;
            let name = row.string("name")?;
            let kind = match row.code("relkind")? {
                "S" => {
                    b.schema(&schema).sequences.push(name);
                    continue;
                }
                "v" => RelationKind::View,
                "m" => RelationKind::MatView,
                "f" => RelationKind::Foreign,
                _ if row.code("persistence")? == "t" => RelationKind::Temporary,
                _ => RelationKind::Table,
            };
            let comment = cut_comment(row.opt_string("comment")?);
            b.relation(&schema, name, kind, comment);
        }

        let limit = i64::try_from(scope.max_columns.saturating_add(1)).unwrap_or(i64::MAX);
        let column_params = [system, schemas, relations, Value::Int(limit)];
        let columns = query(c, COLUMNS, &column_params, &cancel).await?;
        for (read, row) in columns.iter().enumerate() {
            if read >= scope.max_columns {
                snap.truncated = true;
                break;
            }
            let schema = row.string("schema_name")?;
            let rel = row.string("rel")?;
            let column = CatalogColumn {
                name: row.string("name")?,
                native_type: row.opt_string("native_type")?.unwrap_or_default(),
                nullable: row.flag("nullable")?,
                primary_key: row.flag("primary_key")?,
            };
            if let Some(r) = b.relation_mut(&schema, &rel) {
                r.columns.push(column);
            }
        }

        let mut last_fk: Option<i64> = None;
        for row in query(c, FOREIGN_KEYS, &schema_params, &cancel)
            .await?
            .iter()
        {
            let schema = row.string("schema_name")?;
            let rel = row.string("rel")?;
            let id = row.i64("id")?;
            let col = row.string("col")?;
            let ref_col = row.string("ref_col")?;
            let ref_schema = row.string("ref_schema")?;
            let ref_table = row.string("ref_table")?;
            let Some(r) = b.relation_mut(&schema, &rel) else {
                continue;
            };
            if last_fk == Some(id)
                && let Some(fk) = r.foreign_keys.last_mut()
            {
                fk.columns.push(col);
                fk.ref_columns.push(ref_col);
                continue;
            }
            last_fk = Some(id);
            r.foreign_keys.push(CatalogForeignKey {
                columns: vec![col],
                ref_schema: (ref_schema != schema).then_some(ref_schema),
                ref_table,
                ref_columns: vec![ref_col],
            });
        }

        if !relations_only {
            for row in query(c, routines_sql(&version), &schema_params, &cancel)
                .await?
                .iter()
            {
                let schema = row.string("schema_name")?;
                let kind = match row.code("kind")? {
                    "p" => RoutineKind::Procedure,
                    "a" => RoutineKind::Aggregate,
                    _ => RoutineKind::Function,
                };
                let routine = CatalogRoutine {
                    name: row.string("name")?,
                    kind,
                    args: row.opt_string("args")?.unwrap_or_default(),
                    returns: if kind == RoutineKind::Procedure {
                        None
                    } else {
                        row.opt_string("returns")?.filter(|r| !r.is_empty())
                    },
                };
                b.schema(&schema).routines.push(routine);
            }
            for row in query(c, TYPES, &schema_params, &cancel).await?.iter() {
                let schema = row.string("schema_name")?;
                let name = row.string("name")?;
                b.schema(&schema).types.push(name);
            }
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }

        snap.schemas = b.schemas.into_values().collect();
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
    fn scope_lists_quote_names() {
        let scope = CatalogScope {
            schemas: Some(vec!["a\"b".into(), "Sales".into()]),
            relations: Some(vec![
                RelationName {
                    schema: None,
                    name: "t".into(),
                },
                RelationName {
                    schema: Some("s".into()),
                    name: "x\"y".into(),
                },
            ]),
            include_system: true,
            max_columns: 10,
        };
        let [system, schemas, relations] = scope_params(&scope, "public");
        assert_eq!(system, Value::Bool(true));
        assert_eq!(schemas, Value::Text("\"a\"\"b\"\"Sales\"".into()));
        assert_eq!(
            relations,
            Value::Text("\"public\".\"t\"\"s\".\"x\"\"y\"".into())
        );
        let [_, schemas, relations] = scope_params(&CatalogScope::full(), "public");
        assert_eq!((schemas, relations), (Value::Null, Value::Null));
    }

    #[test]
    fn queries_bind_every_name() {
        for sql in [
            SCHEMAS,
            RELATIONS,
            COLUMNS,
            FOREIGN_KEYS,
            ROUTINES,
            ROUTINES_10,
            TYPES,
        ] {
            assert!(sql.contains("$1::bool"), "{sql}");
            assert!(sql.contains("$2::text"), "{sql}");
            assert!(sql.contains("$3::text"), "{sql}");
        }
        assert!(COLUMNS.contains("$4::int8"));
        assert_eq!(
            routines_sql(&ServerVersion::new(10, 23, 0)),
            ROUTINES_10,
            "PostgreSQL 10 has no prokind"
        );
        assert_eq!(routines_sql(&ServerVersion::new(18, 0, 0)), ROUTINES);
    }

    #[test]
    fn comments_are_cut() {
        let long = "x".repeat(CatalogSnapshot::MAX_COMMENT_CHARS + 20);
        assert_eq!(
            cut_comment(Some(long)).map(|c| c.chars().count()),
            Some(CatalogSnapshot::MAX_COMMENT_CHARS)
        );
        assert_eq!(cut_comment(Some(String::new())), None);
    }
}
