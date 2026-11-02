//! M6: the catalog snapshot behind code insight (`docs/m6-contracts.md` §3).
//!
//! A [`CatalogSnapshot`] is plain data: every schema-level name of one database (schemas,
//! tables / views with their columns, keys and foreign keys, routines, sequences and types) as
//! one engine's `CatalogReader` (`oxbase_dialect::catalog`) read it in a fixed, small number of
//! catalog queries. `oxbase-schema-cache` builds its lookup indices from it, and the app
//! database persists it (`oxbase_store::SchemaCacheRepository`) so the editor has names at
//! startup before any connection is open.
//!
//! [`IdentCase`] and [`CaseRules`] say how an identifier typed in SQL matches a catalog name on
//! the engine the snapshot came from (PostgreSQL folds unquoted names to lower case, MySQL table
//! names depend on `lower_case_table_names`, SQLite compares case-insensitively, SQL Server
//! follows the database collation).

use serde::{Deserialize, Serialize};

use crate::{Engine, ServerVersion};

/// How an identifier written in SQL is compared with a catalog name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentCase {
    /// Unquoted identifiers fold to lower case (ASCII), quoted ones match exactly:
    /// PostgreSQL.
    FoldLower,
    /// Unquoted identifiers fold to upper case (ASCII), quoted ones match exactly: the SQL
    /// standard (no v1 engine; kept for completeness).
    FoldUpper,
    /// Quoted or not, names match ignoring case: SQLite, SQL Server with a case-insensitive
    /// collation, MySQL / MariaDB column names, and table names with
    /// `lower_case_table_names` 1 or 2.
    Insensitive,
    /// Quoted or not, names match exactly: MySQL / MariaDB table names with
    /// `lower_case_table_names = 0`, SQL Server with a case-sensitive collation.
    Sensitive,
}

impl IdentCase {
    /// `true` when `written` (the identifier's text without quotes; `quoted` says whether it
    /// was quoted) names the catalog object `catalog_name` under this rule.
    pub fn matches(self, written: &str, quoted: bool, catalog_name: &str) -> bool {
        match self {
            Self::FoldLower if !quoted => fold_eq(written, catalog_name, u8::to_ascii_lowercase),
            Self::FoldUpper if !quoted => fold_eq(written, catalog_name, u8::to_ascii_uppercase),
            Self::FoldLower | Self::FoldUpper | Self::Sensitive => written == catalog_name,
            Self::Insensitive => {
                written.eq_ignore_ascii_case(catalog_name)
                    || (!written.is_ascii()
                        && written.to_lowercase() == catalog_name.to_lowercase())
            }
        }
    }

    /// The catalog spelling an unquoted identifier stands for, when the rule folds it
    /// (`FoldLower` / `FoldUpper`); otherwise the text unchanged.
    pub fn fold_unquoted(self, written: &str) -> String {
        match self {
            Self::FoldLower => written.to_ascii_lowercase(),
            Self::FoldUpper => written.to_ascii_uppercase(),
            Self::Insensitive | Self::Sensitive => written.to_owned(),
        }
    }

    /// `true` when `name` keeps its spelling when written without quotes (ignoring reserved
    /// words and characters, which the dialect checks): under `FoldLower` a name with an
    /// upper-case ASCII letter needs quotes, under `FoldUpper` one with a lower-case letter.
    pub fn case_survives_unquoted(self, name: &str) -> bool {
        match self {
            Self::FoldLower => !name.bytes().any(|b| b.is_ascii_uppercase()),
            Self::FoldUpper => !name.bytes().any(|b| b.is_ascii_lowercase()),
            Self::Insensitive | Self::Sensitive => true,
        }
    }
}

fn fold_eq(written: &str, catalog_name: &str, fold: fn(&u8) -> u8) -> bool {
    written.len() == catalog_name.len()
        && written
            .bytes()
            .zip(catalog_name.bytes())
            .all(|(w, c)| fold(&w) == c)
}

/// The [`IdentCase`] of each name class on one database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CaseRules {
    /// Databases and schemas.
    pub schemas: IdentCase,
    /// Tables, views, sequences and types.
    pub relations: IdentCase,
    /// Columns (and aliases).
    pub columns: IdentCase,
    /// Functions and procedures.
    pub routines: IdentCase,
}

impl CaseRules {
    /// One rule for every name class.
    pub const fn uniform(case: IdentCase) -> Self {
        Self {
            schemas: case,
            relations: case,
            columns: case,
            routines: case,
        }
    }

    /// The engine's default rules, before the catalog reader looks at server settings:
    /// PostgreSQL folds to lower case; MySQL / MariaDB compare table and database names
    /// exactly (`lower_case_table_names = 0`, the Linux default) and everything else ignoring
    /// case; SQLite and SQL Server (case-insensitive default collation) ignore case. MongoDB
    /// and Redis compare exactly.
    pub const fn engine_default(engine: Engine) -> Self {
        match engine {
            Engine::Postgres => Self::uniform(IdentCase::FoldLower),
            Engine::MySql | Engine::MariaDb => Self {
                schemas: IdentCase::Sensitive,
                relations: IdentCase::Sensitive,
                columns: IdentCase::Insensitive,
                routines: IdentCase::Insensitive,
            },
            Engine::Sqlite | Engine::MsSql => Self::uniform(IdentCase::Insensitive),
            Engine::MongoDb | Engine::Redis => Self::uniform(IdentCase::Sensitive),
        }
    }
}

/// What kind of row source a [`CatalogRelation`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    Table,
    View,
    MatView,
    /// A foreign table (PostgreSQL) or other external table.
    Foreign,
    /// A temporary table visible to the reading session (rare: caches are read on their own
    /// session, so most temporary tables never appear).
    Temporary,
}

/// A foreign key of a relation, for `JOIN ... ON` completion and hover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogForeignKey {
    pub columns: Vec<String>,
    /// Schema of the referenced table; `None` = the same schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_schema: Option<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

/// One column of a [`CatalogRelation`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogColumn {
    pub name: String,
    /// The type as the engine spells it (`character varying(40)`, `int unsigned`, ...).
    pub native_type: String,
    pub nullable: bool,
    /// Part of the primary key.
    #[serde(default, skip_serializing_if = "is_false")]
    pub primary_key: bool,
}

/// A table, view or other row source with its columns, in ordinal order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogRelation {
    pub name: String,
    pub kind: RelationKind,
    pub columns: Vec<CatalogColumn>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub foreign_keys: Vec<CatalogForeignKey>,
    /// The object comment, cut to [`CatalogSnapshot::MAX_COMMENT_CHARS`] characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// Function or procedure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineKind {
    Function,
    Procedure,
    Aggregate,
}

/// One routine (one entry per overload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogRoutine {
    pub name: String,
    pub kind: RoutineKind,
    /// The argument list for display, without parentheses (`a integer, b text DEFAULT ''`).
    pub args: String,
    /// The return type for display; `None` for procedures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns: Option<String>,
}

/// One schema (PostgreSQL, SQL Server) or the database itself on engines whose databases have
/// no schemas (MySQL, MariaDB: the schema is named after the database; SQLite: `main` and
/// each attached database).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSchema {
    pub name: String,
    /// A system schema (`pg_catalog`, `information_schema`, `sys`, ...).
    #[serde(default, skip_serializing_if = "is_false")]
    pub system: bool,
    /// Sorted by name.
    pub relations: Vec<CatalogRelation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routines: Vec<CatalogRoutine>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sequences: Vec<String>,
    /// User-defined types (enums, domains, composite types, table types).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<String>,
}

impl CatalogSchema {
    /// An empty schema named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            system: false,
            relations: Vec::new(),
            routines: Vec::new(),
            sequences: Vec::new(),
            types: Vec::new(),
        }
    }
}

/// Every schema-level name of one database, as read by a catalog reader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    /// [`CatalogSnapshot::FORMAT`] of the build that wrote it. A persisted snapshot of another
    /// format is discarded and read again.
    pub format: u32,
    pub engine: Engine,
    pub server_version: ServerVersion,
    /// The database this snapshot describes.
    pub database: String,
    /// Other databases on the server the user can see (for `USE` and cross-database names).
    #[serde(default)]
    pub databases: Vec<String>,
    /// The schema unqualified names resolve in first (`public`, `dbo`, the database itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_schema: Option<String>,
    /// Schemas searched for unqualified names, in order (PostgreSQL `search_path` without
    /// `$user` entries that do not exist; elsewhere `[default_schema]`).
    #[serde(default)]
    pub search_path: Vec<String>,
    pub case_rules: CaseRules,
    /// Sorted by name.
    pub schemas: Vec<CatalogSchema>,
    /// When the reader finished (Unix milliseconds, UTC).
    pub read_at_ms: i64,
    /// The reader stopped at a size limit: names are missing, so "unknown name" findings must
    /// stay hints.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

impl CatalogSnapshot {
    /// The snapshot format this build writes.
    pub const FORMAT: u32 = 1;
    /// Comments longer than this many characters are cut by the reader.
    pub const MAX_COMMENT_CHARS: usize = 500;

    /// An empty snapshot of `database` with the engine's default [`CaseRules`].
    pub fn new(engine: Engine, server_version: ServerVersion, database: impl Into<String>) -> Self {
        Self {
            format: Self::FORMAT,
            engine,
            server_version,
            database: database.into(),
            databases: Vec::new(),
            default_schema: None,
            search_path: Vec::new(),
            case_rules: CaseRules::engine_default(engine),
            schemas: Vec::new(),
            read_at_ms: 0,
            truncated: false,
        }
    }

    /// Number of relations in every schema.
    pub fn relation_count(&self) -> usize {
        self.schemas.iter().map(|s| s.relations.len()).sum()
    }

    /// Number of columns of every relation.
    pub fn column_count(&self) -> usize {
        self.schemas
            .iter()
            .flat_map(|s| &s.relations)
            .map(|r| r.columns.len())
            .sum()
    }

    /// Number of routines in every schema.
    pub fn routine_count(&self) -> usize {
        self.schemas.iter().map(|s| s.routines.len()).sum()
    }

    /// Sorts schemas, relations, routines, sequences and types by name (byte order), the
    /// order the cache and the persisted form expect. Columns keep their ordinal order.
    pub fn normalize(&mut self) {
        self.schemas.sort_by(|a, b| a.name.cmp(&b.name));
        for s in &mut self.schemas {
            s.relations.sort_by(|a, b| a.name.cmp(&b.name));
            s.routines
                .sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.args.cmp(&b.args)));
            s.sequences.sort();
            s.types.sort();
        }
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if passes a reference
fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_matching_follows_the_rule() {
        let pg = IdentCase::FoldLower;
        assert!(pg.matches("Orders", false, "orders"));
        assert!(!pg.matches("Orders", true, "orders"));
        assert!(pg.matches("Orders", true, "Orders"));
        assert!(!pg.matches("orders", false, "Orders"));
        let up = IdentCase::FoldUpper;
        assert!(up.matches("orders", false, "ORDERS"));
        let ci = IdentCase::Insensitive;
        assert!(ci.matches("ORDERS", true, "orders"));
        assert!(ci.matches("ÄPFEL", false, "äpfel"));
        let cs = IdentCase::Sensitive;
        assert!(!cs.matches("Orders", false, "orders"));
        assert!(cs.matches("orders", false, "orders"));
    }

    #[test]
    fn folding_and_quoting_needs() {
        assert_eq!(IdentCase::FoldLower.fold_unquoted("AbC"), "abc");
        assert_eq!(IdentCase::Sensitive.fold_unquoted("AbC"), "AbC");
        assert!(IdentCase::FoldLower.case_survives_unquoted("orders_2"));
        assert!(!IdentCase::FoldLower.case_survives_unquoted("Orders"));
        assert!(IdentCase::Insensitive.case_survives_unquoted("Orders"));
        assert!(!IdentCase::FoldUpper.case_survives_unquoted("Orders"));
    }

    #[test]
    fn engine_defaults() {
        assert_eq!(
            CaseRules::engine_default(Engine::Postgres),
            CaseRules::uniform(IdentCase::FoldLower)
        );
        let my = CaseRules::engine_default(Engine::MySql);
        assert_eq!(my.relations, IdentCase::Sensitive);
        assert_eq!(my.columns, IdentCase::Insensitive);
        assert_eq!(
            CaseRules::engine_default(Engine::MsSql).relations,
            IdentCase::Insensitive
        );
    }

    #[test]
    fn snapshot_counts_normalizes_and_round_trips() {
        let mut snap = CatalogSnapshot::new(Engine::Postgres, ServerVersion::new(18, 0, 0), "app");
        let mut s = CatalogSchema::new("public");
        s.relations.push(CatalogRelation {
            name: "b".into(),
            kind: RelationKind::View,
            columns: vec![],
            foreign_keys: vec![],
            comment: None,
        });
        s.relations.push(CatalogRelation {
            name: "a".into(),
            kind: RelationKind::Table,
            columns: vec![
                CatalogColumn {
                    name: "z".into(),
                    native_type: "integer".into(),
                    nullable: false,
                    primary_key: true,
                },
                CatalogColumn {
                    name: "y".into(),
                    native_type: "text".into(),
                    nullable: true,
                    primary_key: false,
                },
            ],
            foreign_keys: vec![CatalogForeignKey {
                columns: vec!["y".into()],
                ref_schema: None,
                ref_table: "b".into(),
                ref_columns: vec!["k".into()],
            }],
            comment: Some("first".into()),
        });
        s.routines.push(CatalogRoutine {
            name: "f".into(),
            kind: RoutineKind::Function,
            args: "x integer".into(),
            returns: Some("integer".into()),
        });
        snap.schemas.push(CatalogSchema::new("z_last"));
        snap.schemas.push(s);
        snap.normalize();
        assert_eq!(snap.schemas[0].name, "public");
        assert_eq!(snap.schemas[0].relations[0].name, "a");
        // Columns keep their ordinal order.
        assert_eq!(snap.schemas[0].relations[0].columns[0].name, "z");
        assert_eq!(
            (
                snap.relation_count(),
                snap.column_count(),
                snap.routine_count()
            ),
            (2, 2, 1)
        );
        let json = serde_json::to_string(&snap).unwrap();
        let back: CatalogSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap);
        // Defaults are left out of the persisted form.
        assert!(!json.contains("\"truncated\""), "{json}");
        assert!(!json.contains("\"system\""), "{json}");
    }
}
