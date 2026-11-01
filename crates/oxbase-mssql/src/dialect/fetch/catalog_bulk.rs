//! M6: the SQL Server [`CatalogReader`] behind the schema cache (`docs/m6-contracts.md` §3.2).
//!
//! A full read is eight catalog queries of the session's database whatever the number of
//! tables: settings (database, default schema, collation), databases, schemas, objects (tables,
//! views, routines, sequences), columns (with primary-key flags), foreign keys, routine
//! parameters and user-defined types. Every query is fixed text and binds the same five
//! parameters:
//!
//! - `@P1` (`1` = everything), `@P2` (the scope's schemas as `"name"` entries, quotes doubled),
//!   `@P3` (the scope's relations as `"schema"."name"` entries); the lists are searched with
//!   `CHARINDEX` on lower-cased text, so they can only match more than asked for (another
//!   case of the same name), and the cache merges what comes back;
//! - `@P4` (`1` = include `sys` / `INFORMATION_SCHEMA` and objects the server ships);
//! - `@P5` (the column limit plus one).
//!
//! The case rule of every name class follows the database collation: `_CS` and binary
//! collations compare exactly, everything else ignores case.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use oxbase_dialect::{CatalogReader, CatalogScope};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    CaseRules, CatalogColumn, CatalogForeignKey, CatalogRelation, CatalogRoutine, CatalogSchema,
    CatalogSnapshot, Engine, Error, IdentCase, RelationKind, Result, RoutineKind, Value,
};

use super::is_system_schema;
use super::rows::{Row, query};
use super::types::system_spelling;

/// The scope filter on a schema-level object in schema `s`.
macro_rules! schema_filter {
    () => {
        "(@P1 = 1 OR CHARINDEX(LOWER(N'\"' + REPLACE(s.name, N'\"', N'\"\"') + N'\"'), \
                               LOWER(@P2)) > 0)"
    };
}

/// The scope filter on object `o` in schema `s`.
macro_rules! relation_filter {
    () => {
        "(@P1 = 1 OR CHARINDEX(LOWER(N'\"' + REPLACE(s.name, N'\"', N'\"\"') + N'\"'), \
                               LOWER(@P2)) > 0 \
          OR CHARINDEX(LOWER(N'\"' + REPLACE(s.name, N'\"', N'\"\"') + N'\".\"' \
                             + REPLACE(o.name, N'\"', N'\"\"') + N'\"'), LOWER(@P3)) > 0)"
    };
}

/// Hides `sys`, `INFORMATION_SCHEMA` and shipped objects unless `@P4 = 1`.
macro_rules! system_filter {
    () => {
        "(@P4 = 1 OR (o.is_ms_shipped = 0 AND s.name NOT IN (N'sys', N'INFORMATION_SCHEMA')))"
    };
}

const SETTINGS: &str = "\
SELECT DB_NAME() AS database_name, SCHEMA_NAME() AS default_schema, \
       CAST(DATABASEPROPERTYEX(DB_NAME(), 'Collation') AS nvarchar(128)) AS collation_name";

const DATABASES: &str = "\
SELECT name FROM sys.databases WHERE HAS_DBACCESS(name) = 1 AND state = 0 ORDER BY name";

const SCHEMAS: &str = concat!(
    "SELECT s.name, CAST(s.schema_id AS int) AS schema_id FROM sys.schemas s WHERE ",
    schema_filter!()
);

const OBJECTS: &str = concat!(
    "SELECT s.name AS schema_name, o.name, RTRIM(o.type) AS type, \
            CAST(ep.value AS nvarchar(4000)) AS comment \
     FROM sys.all_objects o JOIN sys.schemas s ON s.schema_id = o.schema_id \
     LEFT JOIN sys.extended_properties ep ON ep.class = 1 AND ep.major_id = o.object_id \
          AND ep.minor_id = 0 AND ep.name = N'MS_Description' \
     WHERE o.type IN ('U', 'V', 'FN', 'IF', 'TF', 'FS', 'FT', 'AF', 'P', 'PC', 'SO') AND ",
    relation_filter!(),
    " AND ",
    system_filter!()
);

const COLUMNS: &str = concat!(
    "SELECT TOP (@P5) s.name AS schema_name, o.name AS rel, c.name, t.name AS type_name, \
            ts.name AS type_schema, t.is_user_defined, CAST(c.max_length AS int) AS max_length, \
            CAST(c.precision AS int) AS num_precision, CAST(c.scale AS int) AS scale, \
            c.is_nullable, \
            CAST(CASE WHEN ic.column_id IS NULL THEN 0 ELSE 1 END AS bit) AS in_primary_key \
     FROM sys.all_columns c \
     JOIN sys.all_objects o ON o.object_id = c.object_id \
     JOIN sys.schemas s ON s.schema_id = o.schema_id \
     JOIN sys.types t ON t.user_type_id = c.user_type_id \
     JOIN sys.schemas ts ON ts.schema_id = t.schema_id \
     LEFT JOIN sys.indexes pk ON pk.object_id = o.object_id AND pk.is_primary_key = 1 \
     LEFT JOIN sys.index_columns ic ON ic.object_id = pk.object_id AND ic.index_id = pk.index_id \
          AND ic.column_id = c.column_id \
     WHERE o.type IN ('U', 'V') AND ",
    relation_filter!(),
    " AND ",
    system_filter!(),
    " ORDER BY o.object_id, c.column_id"
);

const FOREIGN_KEYS: &str = concat!(
    "SELECT s.name AS schema_name, o.name AS rel, CAST(fkc.constraint_object_id AS int) AS fk_id, \
            rs.name AS ref_schema, ro.name AS ref_table, pc.name AS col, rc.name AS ref_col \
     FROM sys.foreign_key_columns fkc \
     JOIN sys.objects o ON o.object_id = fkc.parent_object_id \
     JOIN sys.schemas s ON s.schema_id = o.schema_id \
     JOIN sys.objects ro ON ro.object_id = fkc.referenced_object_id \
     JOIN sys.schemas rs ON rs.schema_id = ro.schema_id \
     JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id \
          AND pc.column_id = fkc.parent_column_id \
     JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id \
          AND rc.column_id = fkc.referenced_column_id \
     WHERE ",
    relation_filter!(),
    " AND ",
    system_filter!(),
    " ORDER BY fkc.constraint_object_id, fkc.constraint_column_id"
);

const PARAMETERS: &str = concat!(
    "SELECT s.name AS schema_name, o.name AS routine, p.name AS param, t.name AS type_name, \
            ts.name AS type_schema, t.is_user_defined, CAST(p.max_length AS int) AS max_length, \
            CAST(p.precision AS int) AS num_precision, CAST(p.scale AS int) AS scale, \
            p.is_output, CAST(p.parameter_id AS int) AS parameter_id \
     FROM sys.all_parameters p \
     JOIN sys.all_objects o ON o.object_id = p.object_id \
     JOIN sys.schemas s ON s.schema_id = o.schema_id \
     JOIN sys.types t ON t.user_type_id = p.user_type_id \
     JOIN sys.schemas ts ON ts.schema_id = t.schema_id \
     WHERE o.type IN ('FN', 'IF', 'TF', 'FS', 'FT', 'AF', 'P', 'PC') AND ",
    schema_filter!(),
    " AND ",
    system_filter!(),
    " ORDER BY o.object_id, p.parameter_id"
);

const TYPES: &str = concat!(
    "SELECT s.name AS schema_name, t.name FROM sys.types t \
     JOIN sys.schemas s ON s.schema_id = t.schema_id WHERE t.is_user_defined = 1 AND ",
    schema_filter!()
);

/// `"a""b"` entries, the list form `CHARINDEX` looks names up in.
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

/// The bound `@P1`-`@P5` of a scope; `default_schema` stands in for relations without one.
fn scope_params(scope: &CatalogScope, default_schema: &str) -> Vec<Value> {
    let schemas: String = scope.schemas.iter().flatten().map(|s| quoted(s)).collect();
    let relations: String = scope
        .relations
        .iter()
        .flatten()
        .map(|r| {
            let schema = r.schema.as_deref().unwrap_or(default_schema);
            qualified(schema, &r.name)
        })
        .collect();
    let limit = i64::try_from(scope.max_columns.saturating_add(1)).unwrap_or(i64::MAX);
    vec![
        Value::Int(i64::from(scope.is_full())),
        Value::Text(schemas),
        Value::Text(relations),
        Value::Int(i64::from(scope.include_system)),
        Value::Int(limit),
    ]
}

/// The case rules of a database collation: `_CS` and binary collations compare exactly.
fn case_rules(collation: &str) -> CaseRules {
    let exact = collation
        .split('_')
        .any(|p| p.eq_ignore_ascii_case("CS") || p.to_ascii_uppercase().starts_with("BIN"));
    CaseRules::uniform(if exact {
        IdentCase::Sensitive
    } else {
        IdentCase::Insensitive
    })
}

/// The type of a column or parameter row as DDL writes it.
fn native_type(row: &Row<'_>) -> Result<String> {
    let name = row.string("type_name")?;
    if row.flag("is_user_defined")? {
        let schema = row.opt_string("type_schema")?.unwrap_or_default();
        return Ok(format!("{schema}.{name}"));
    }
    Ok(system_spelling(
        &name,
        row.opt_i64("max_length")?.unwrap_or(0),
        row.opt_i64("num_precision")?.unwrap_or(0),
        row.opt_i64("scale")?.unwrap_or(0),
    ))
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

/// Gathers the rows of the catalog queries into schemas.
#[derive(Default)]
struct Builder {
    schemas: BTreeMap<String, CatalogSchema>,
    relations: HashMap<(String, String), usize>,
    /// (schema, routine) → index in that schema's `routines`.
    routines: HashMap<(String, String), usize>,
}

impl Builder {
    fn schema(&mut self, name: &str) -> &mut CatalogSchema {
        self.schemas
            .entry(name.to_owned())
            .or_insert_with(|| CatalogSchema::new(name))
    }

    fn relation_mut(&mut self, schema: &str, name: &str) -> Option<&mut CatalogRelation> {
        let index = *self.relations.get(&(schema.to_owned(), name.to_owned()))?;
        self.schemas.get_mut(schema)?.relations.get_mut(index)
    }
}

/// The SQL Server catalog reader. See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct MsSqlCatalogReader;

#[async_trait]
impl CatalogReader for MsSqlCatalogReader {
    async fn read(
        &self,
        c: &dyn Connection,
        scope: &CatalogScope,
        cancel: CancellationToken,
    ) -> Result<CatalogSnapshot> {
        let settings = query(c, SETTINGS, &[], &cancel).await?;
        let (database, default_schema, collation) = match settings.first() {
            Some(row) => (
                row.opt_string("database_name")?.unwrap_or_default(),
                row.opt_string("default_schema")?,
                row.opt_string("collation_name")?.unwrap_or_default(),
            ),
            None => (String::new(), None, String::new()),
        };
        let mut snap =
            CatalogSnapshot::new(Engine::MsSql, c.server_version().clone(), database.clone());
        snap.case_rules = case_rules(&collation);
        let default_schema = default_schema.unwrap_or_else(|| "dbo".to_owned());
        snap.default_schema = Some(default_schema.clone());
        snap.search_path = vec![default_schema.clone()];
        snap.databases = query(c, DATABASES, &[], &cancel)
            .await?
            .iter()
            .map(|r| r.string("name"))
            .collect::<Result<_>>()?;

        let params = scope_params(scope, &default_schema);
        let relations_only = scope.schemas.is_none() && scope.relations.is_some();
        let mut b = Builder::default();
        if !relations_only {
            for row in query(c, SCHEMAS, &params, &cancel).await?.iter() {
                let name = row.string("name")?;
                let system = is_system_schema(&name, row.opt_i64("schema_id")?);
                let wanted = !system
                    || scope.include_system
                    || scope.schemas.iter().flatten().any(|s| s == &name);
                if wanted {
                    b.schema(&name).system = system;
                }
            }
        }
        for row in query(c, OBJECTS, &params, &cancel).await?.iter() {
            let schema = row.string("schema_name")?;
            let name = row.string("name")?;
            let code = row.code("type")?;
            let kind = match code {
                "U" => RelationKind::Table,
                "V" => RelationKind::View,
                _ if relations_only => continue,
                "SO" => {
                    b.schema(&schema).sequences.push(name);
                    continue;
                }
                _ => {
                    let kind = match code {
                        "P" | "PC" => RoutineKind::Procedure,
                        "AF" => RoutineKind::Aggregate,
                        _ => RoutineKind::Function,
                    };
                    let returns = matches!(code, "IF" | "TF" | "FT").then(|| "TABLE".to_owned());
                    let s = b.schema(&schema);
                    s.routines.push(CatalogRoutine {
                        name: name.clone(),
                        kind,
                        args: String::new(),
                        returns,
                    });
                    let index = s.routines.len() - 1;
                    b.routines.insert((schema, name), index);
                    continue;
                }
            };
            let comment = cut_comment(row.opt_string("comment")?);
            let s = b.schema(&schema);
            s.relations.push(CatalogRelation {
                name: name.clone(),
                kind,
                columns: Vec::new(),
                foreign_keys: Vec::new(),
                comment,
            });
            let index = s.relations.len() - 1;
            b.relations.insert((schema, name), index);
        }

        for (read, row) in query(c, COLUMNS, &params, &cancel)
            .await?
            .iter()
            .enumerate()
        {
            if read >= scope.max_columns {
                snap.truncated = true;
                break;
            }
            let schema = row.string("schema_name")?;
            let rel = row.string("rel")?;
            let column = CatalogColumn {
                name: row.string("name")?,
                native_type: native_type(&row)?,
                nullable: row.flag("is_nullable")?,
                primary_key: row.flag("in_primary_key")?,
            };
            if let Some(r) = b.relation_mut(&schema, &rel) {
                r.columns.push(column);
            }
        }

        let mut last_fk: Option<i64> = None;
        for row in query(c, FOREIGN_KEYS, &params, &cancel).await?.iter() {
            let schema = row.string("schema_name")?;
            let rel = row.string("rel")?;
            let id = row.i64("fk_id")?;
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
            for row in query(c, PARAMETERS, &params, &cancel).await?.iter() {
                let schema = row.string("schema_name")?;
                let routine = row.string("routine")?;
                let Some(&index) = b.routines.get(&(schema.clone(), routine)) else {
                    continue;
                };
                let ty = native_type(&row)?;
                let Some(r) = b
                    .schemas
                    .get_mut(&schema)
                    .and_then(|s| s.routines.get_mut(index))
                else {
                    continue;
                };
                if row.i64("parameter_id")? == 0 {
                    if r.returns.is_none() {
                        r.returns = Some(ty);
                    }
                    continue;
                }
                let name = row.opt_string("param")?.unwrap_or_default();
                let output = if row.flag("is_output")? {
                    " OUTPUT"
                } else {
                    ""
                };
                if !r.args.is_empty() {
                    r.args.push_str(", ");
                }
                let _ = write!(r.args, "{name} {ty}{output}");
            }
            for row in query(c, TYPES, &params, &cancel).await?.iter() {
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
    fn collations_decide_the_case_rule() {
        assert_eq!(
            case_rules("SQL_Latin1_General_CP1_CI_AS").relations,
            IdentCase::Insensitive
        );
        assert_eq!(
            case_rules("Latin1_General_100_CS_AS_SC_UTF8").relations,
            IdentCase::Sensitive
        );
        assert_eq!(
            case_rules("Latin1_General_BIN2").columns,
            IdentCase::Sensitive
        );
        assert_eq!(case_rules("").schemas, IdentCase::Insensitive);
    }

    #[test]
    fn scope_parameters() {
        let full = scope_params(&CatalogScope::full(), "dbo");
        assert_eq!(full[0], Value::Int(1));
        assert_eq!(full[3], Value::Int(0));
        let part = scope_params(
            &CatalogScope {
                relations: Some(vec![RelationName {
                    schema: None,
                    name: "a\"b".into(),
                }]),
                max_columns: 9,
                ..CatalogScope::full()
            },
            "dbo",
        );
        assert_eq!(part[0], Value::Int(0));
        assert_eq!(part[2], Value::Text("\"dbo\".\"a\"\"b\"".into()));
        assert_eq!(part[4], Value::Int(10));
    }

    #[test]
    fn queries_use_the_bound_scope() {
        for sql in [SCHEMAS, OBJECTS, COLUMNS, FOREIGN_KEYS, PARAMETERS, TYPES] {
            assert!(sql.contains("@P1 = 1"), "{sql}");
            assert!(sql.contains("LOWER(@P2)"), "{sql}");
        }
        assert!(COLUMNS.contains("TOP (@P5)"));
    }
}
