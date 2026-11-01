//! Catalog queries of the SQL Server fetchers.
//!
//! Every query is a `&'static str` fixed at compile time: names from object references only
//! travel as parameters (`@P1`, `@P2`, bound as `nvarchar` through `sp_executesql`) and object
//! ids as `@P1` (`bigint`), never as SQL text (docs/m4-contracts.md §3.5). Queries read the
//! session database's `sys.*` catalog views; only [`DATABASES`] / [`DATABASE`] read the
//! server-wide `sys.databases`.
//!
//! Columns are shaped for the driver's Arrow mapping (§2.5): flags are `bit` (`Boolean`), small
//! integers come back as `Int64`, `sql_variant` values (extended properties, identity seeds,
//! sequence bounds) are cast to `nvarchar` so their text is exact, and `char(2)` object types are
//! `RTRIM`med.
//!
//! Version gates (§3.5): SQL Server 2017 (14.x) is the oldest supported server, so the temporal
//! (`temporal_type`, `history_table_id`), graph (`is_node`, `is_edge`), memory-optimized and
//! period-column (`generated_always_type`, `is_hidden`) columns are always present;
//! `sys.tables.ledger_type` needs 2022 (16.x): [`tables`] and [`table_head`] pick the query.
//! Results are sorted in Rust (binary order of the names), not with `ORDER BY` under the
//! database collation.

use oxbase_types::ServerVersion;

/// `true` when `v` is at least `major` (an unknown `0.0.0` version counts as current).
pub(crate) fn at_least(v: &ServerVersion, major: u32) -> bool {
    v.major == 0 || v.at_least(major, 0)
}

/// The `MS_Description` extended property of class `$class` on `$major` (`minor_id = 0`).
macro_rules! description {
    ($class:literal, $major:literal) => {
        concat!(
            "CAST((SELECT ep.value FROM sys.extended_properties ep WHERE ep.class = ",
            $class,
            " AND ep.major_id = ",
            $major,
            " AND ep.minor_id = 0 AND ep.name = N'MS_Description') AS nvarchar(4000))"
        )
    };
}

/// Every database of the server (system databases included; the fetcher flags them).
pub(crate) const DATABASES: &str = "\
SELECT d.name, CAST(d.database_id AS int) AS database_id, d.state_desc, d.collation_name, \
       CAST(CASE WHEN d.name = DB_NAME() THEN 1 ELSE 0 END AS bit) AS is_current \
FROM sys.databases d";

/// One database by name (`@P1`). The comment is only readable for the session's database.
pub(crate) const DATABASE: &str = concat!(
    "SELECT d.name, CAST(d.database_id AS int) AS database_id, SUSER_SNAME(d.owner_sid) AS owner, \
       d.collation_name, CAST(d.compatibility_level AS int) AS compatibility_level, \
       d.recovery_model_desc, d.state_desc, \
       CAST(CASE WHEN d.name = DB_NAME() THEN 1 ELSE 0 END AS bit) AS is_current, \
       CASE WHEN d.name = DB_NAME() THEN ",
    description!("0", "0"),
    " END AS comment \
FROM sys.databases d WHERE d.name = @P1"
);

/// Every schema of the session's database.
pub(crate) const SCHEMAS: &str = concat!(
    "SELECT s.name, CAST(s.schema_id AS int) AS schema_id, p.name AS owner, ",
    description!("3", "s.schema_id"),
    " AS comment \
FROM sys.schemas s LEFT JOIN sys.database_principals p ON p.principal_id = s.principal_id"
);

/// One schema by name (`@P1`).
pub(crate) const SCHEMA: &str = concat!(
    "SELECT s.name, CAST(s.schema_id AS int) AS schema_id, p.name AS owner, ",
    description!("3", "s.schema_id"),
    " AS comment \
FROM sys.schemas s LEFT JOIN sys.database_principals p ON p.principal_id = s.principal_id \
WHERE s.name = @P1"
);

/// A schema-scoped object by schema (`@P1`) and name (`@P2`): its id, name and type.
pub(crate) const OBJECT: &str = "\
SELECT o.object_id, o.name, RTRIM(o.type) AS type, o.is_ms_shipped \
FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id \
WHERE s.name = @P1 AND o.name = @P2 AND o.parent_object_id = 0";

/// Table columns shared by [`tables`] and [`table_head`]; `$ledger` is the ledger column.
macro_rules! table_columns {
    ($ledger:literal) => {
        concat!(
            "t.object_id, t.name, t.is_ms_shipped, CAST(t.temporal_type AS int) AS temporal_type, \
             t.is_memory_optimized, t.is_node, t.is_edge, ",
            $ledger,
            " AS ledger_type, ",
            description!("1", "t.object_id"),
            " AS comment, \
             (SELECT SUM(p.rows) FROM sys.partitions p \
               WHERE p.object_id = t.object_id AND p.index_id IN (0, 1)) AS row_estimate, \
             (SELECT TOP (1) p.data_compression_desc FROM sys.partitions p \
               WHERE p.object_id = t.object_id AND p.index_id IN (0, 1) \
               ORDER BY p.partition_number) AS data_compression, \
             OBJECT_SCHEMA_NAME(t.history_table_id) AS history_schema, \
             OBJECT_NAME(t.history_table_id) AS history_name"
        )
    };
}

const TABLES_2022: &str = concat!(
    "SELECT ",
    table_columns!("CAST(t.ledger_type AS int)"),
    " FROM sys.tables t JOIN sys.schemas s ON s.schema_id = t.schema_id WHERE s.name = @P1"
);

const TABLES_2017: &str = concat!(
    "SELECT ",
    table_columns!("CAST(0 AS int)"),
    " FROM sys.tables t JOIN sys.schemas s ON s.schema_id = t.schema_id WHERE s.name = @P1"
);

const TABLE_HEAD_2022: &str = concat!(
    "SELECT ",
    table_columns!("CAST(t.ledger_type AS int)"),
    " FROM sys.tables t WHERE t.object_id = @P1"
);

const TABLE_HEAD_2017: &str = concat!(
    "SELECT ",
    table_columns!("CAST(0 AS int)"),
    " FROM sys.tables t WHERE t.object_id = @P1"
);

/// The tables of schema `@P1`.
pub(crate) fn tables(v: &ServerVersion) -> &'static str {
    if at_least(v, 16) {
        TABLES_2022
    } else {
        TABLES_2017
    }
}

/// The table with object id `@P1`.
pub(crate) fn table_head(v: &ServerVersion) -> &'static str {
    if at_least(v, 16) {
        TABLE_HEAD_2022
    } else {
        TABLE_HEAD_2017
    }
}

/// The views of schema `@P1`.
pub(crate) const VIEWS: &str = concat!(
    "SELECT v.name, v.is_ms_shipped, ",
    description!("1", "v.object_id"),
    " AS comment FROM sys.views v JOIN sys.schemas s ON s.schema_id = v.schema_id \
WHERE s.name = @P1"
);

/// One view by schema (`@P1`) and name (`@P2`), with its module.
pub(crate) const VIEW: &str = concat!(
    "SELECT v.object_id, v.name, v.is_ms_shipped, v.with_check_option, m.definition, \
       m.is_schema_bound, ",
    description!("1", "v.object_id"),
    " AS comment \
FROM sys.views v JOIN sys.schemas s ON s.schema_id = v.schema_id \
LEFT JOIN sys.sql_modules m ON m.object_id = v.object_id \
WHERE s.name = @P1 AND v.name = @P2"
);

/// `EXECUTE AS` of a module (`m`: `sys.sql_modules`, `am`: `sys.assembly_modules`); NULL for
/// the default (`CALLER`).
macro_rules! execute_as {
    () => {
        "CASE WHEN COALESCE(m.execute_as_principal_id, am.execute_as_principal_id) IS NULL THEN NULL \
              WHEN COALESCE(m.execute_as_principal_id, am.execute_as_principal_id) = -2 THEN N'OWNER' \
              ELSE USER_NAME(COALESCE(m.execute_as_principal_id, am.execute_as_principal_id)) END"
    };
}

/// The functions of schema `@P1`.
pub(crate) const FUNCTIONS: &str = concat!(
    "SELECT o.name, RTRIM(o.type) AS type, o.is_ms_shipped, ",
    description!("1", "o.object_id"),
    " AS comment FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id \
WHERE s.name = @P1 AND o.type IN ('FN', 'IF', 'TF', 'FS', 'FT', 'AF')"
);

/// The procedures of schema `@P1`.
pub(crate) const PROCEDURES: &str = concat!(
    "SELECT o.name, RTRIM(o.type) AS type, o.is_ms_shipped, ",
    description!("1", "o.object_id"),
    " AS comment FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id \
WHERE s.name = @P1 AND o.type IN ('P', 'PC')"
);

/// One function or procedure by schema (`@P1`) and name (`@P2`), with its module.
pub(crate) const ROUTINE: &str = concat!(
    "SELECT o.object_id, o.name, RTRIM(o.type) AS type, o.is_ms_shipped, m.definition, \
       m.is_schema_bound, ",
    execute_as!(),
    " AS execute_as, ",
    description!("1", "o.object_id"),
    " AS comment \
FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id \
LEFT JOIN sys.sql_modules m ON m.object_id = o.object_id \
LEFT JOIN sys.assembly_modules am ON am.object_id = o.object_id \
WHERE s.name = @P1 AND o.name = @P2 \
  AND o.type IN ('FN', 'IF', 'TF', 'FS', 'FT', 'AF', 'P', 'PC')"
);

/// Type facts shared by columns and parameters (`t`: the type, `ts`: its schema, `bt`: the
/// system base type of an alias type).
macro_rules! type_columns {
    () => {
        "t.name AS type_name, ts.name AS type_schema, t.is_user_defined, t.is_assembly_type, \
         t.is_table_type, bt.name AS base_type"
    };
}

macro_rules! type_joins {
    ($owner:literal) => {
        concat!(
            " JOIN sys.types t ON t.user_type_id = ",
            $owner,
            ".user_type_id JOIN sys.schemas ts ON ts.schema_id = t.schema_id \
              LEFT JOIN sys.types bt ON bt.user_type_id = t.system_type_id \
                                    AND bt.is_user_defined = 0 AND bt.user_type_id <> 256"
        )
    };
}

/// The parameters of routine `@P1` (object id) in order; `parameter_id = 0` is a scalar
/// function's return type.
pub(crate) const PARAMETERS: &str = concat!(
    "SELECT CAST(p.parameter_id AS int) AS parameter_id, p.name, ",
    type_columns!(),
    ", CAST(p.max_length AS int) AS max_length, CAST(p.precision AS int) AS num_precision, \
       CAST(p.scale AS int) AS scale, p.is_output, p.is_readonly \
FROM sys.parameters p",
    type_joins!("p"),
    " WHERE p.object_id = @P1 ORDER BY p.parameter_id"
);

/// The columns of table, view or table type `@P1` (object id) in `column_id` order.
pub(crate) const COLUMNS: &str = concat!(
    "SELECT CAST(c.column_id AS int) AS column_id, c.name, ",
    type_columns!(),
    ", CAST(c.max_length AS int) AS max_length, CAST(c.precision AS int) AS num_precision, \
       CAST(c.scale AS int) AS scale, c.is_nullable, c.is_identity, c.is_computed, c.is_sparse, \
       CASE WHEN c.collation_name COLLATE DATABASE_DEFAULT \
                 <> CAST(DATABASEPROPERTYEX(DB_NAME(), 'Collation') AS nvarchar(128)) \
                    COLLATE DATABASE_DEFAULT \
            THEN c.collation_name END AS collation_name, \
       dc.name AS default_name, dc.definition AS default_definition, \
       CAST(ic.seed_value AS nvarchar(60)) AS identity_seed, \
       CAST(ic.increment_value AS nvarchar(60)) AS identity_increment, \
       ic.is_not_for_replication AS identity_not_for_replication, \
       cc.definition AS computed_definition, cc.is_persisted, \
       CAST(c.generated_always_type AS int) AS generated_always_type, c.is_hidden, \
       CAST((SELECT ep.value FROM sys.extended_properties ep WHERE ep.class = 1 \
             AND ep.major_id = c.object_id AND ep.minor_id = c.column_id \
             AND ep.name = N'MS_Description') AS nvarchar(4000)) AS comment, \
       CAST(CASE WHEN EXISTS (SELECT 1 FROM sys.indexes i \
                              JOIN sys.index_columns k ON k.object_id = i.object_id \
                                                      AND k.index_id = i.index_id \
                              WHERE i.object_id = c.object_id AND i.is_primary_key = 1 \
                                AND k.column_id = c.column_id) \
            THEN 1 ELSE 0 END AS bit) AS in_primary_key \
FROM sys.columns c",
    type_joins!("c"),
    " LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
LEFT JOIN sys.identity_columns ic ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
WHERE c.object_id = @P1 ORDER BY c.column_id"
);

/// The indexes of table, view or table type `@P1` (object id), the heap left out.
pub(crate) const INDEXES: &str = "\
SELECT CAST(i.index_id AS int) AS index_id, i.name, CAST(i.type AS int) AS type, \
       i.is_unique, i.is_primary_key, i.is_unique_constraint, i.filter_definition, \
       CAST(i.fill_factor AS int) AS fill_factor, i.is_disabled, i.ignore_dup_key, \
       (SELECT TOP (1) p.data_compression_desc FROM sys.partitions p \
         WHERE p.object_id = i.object_id AND p.index_id = i.index_id \
         ORDER BY p.partition_number) AS data_compression, \
       CAST((SELECT ep.value FROM sys.extended_properties ep WHERE ep.class = 7 \
             AND ep.major_id = i.object_id AND ep.minor_id = i.index_id \
             AND ep.name = N'MS_Description') AS nvarchar(4000)) AS comment \
FROM sys.indexes i WHERE i.object_id = @P1 AND i.index_id > 0 AND i.is_hypothetical = 0";

/// The index columns of `@P1` (object id), per index in key order then column order.
pub(crate) const INDEX_COLUMNS: &str = "\
SELECT CAST(k.index_id AS int) AS index_id, c.name, CAST(k.key_ordinal AS int) AS key_ordinal, \
       k.is_descending_key, k.is_included_column, \
       CAST(k.index_column_id AS int) AS index_column_id \
FROM sys.index_columns k \
JOIN sys.columns c ON c.object_id = k.object_id AND c.column_id = k.column_id \
WHERE k.object_id = @P1 \
ORDER BY k.index_id, k.key_ordinal, k.index_column_id";

/// The foreign keys of table `@P1` (object id), one row per column pair in order.
pub(crate) const FOREIGN_KEYS: &str = concat!(
    "SELECT fk.object_id AS fk_id, fk.name, \
       CAST(fk.delete_referential_action AS int) AS on_delete, \
       CAST(fk.update_referential_action AS int) AS on_update, \
       fk.is_disabled, fk.is_not_trusted, fk.is_not_for_replication, \
       rs.name AS ref_schema, ro.name AS ref_table, pc.name AS column_name, \
       rc.name AS ref_column_name, ",
    description!("1", "fk.object_id"),
    " AS comment \
FROM sys.foreign_keys fk \
JOIN sys.foreign_key_columns k ON k.constraint_object_id = fk.object_id \
JOIN sys.columns pc ON pc.object_id = k.parent_object_id AND pc.column_id = k.parent_column_id \
JOIN sys.columns rc ON rc.object_id = k.referenced_object_id \
                   AND rc.column_id = k.referenced_column_id \
JOIN sys.objects ro ON ro.object_id = fk.referenced_object_id \
JOIN sys.schemas rs ON rs.schema_id = ro.schema_id \
WHERE fk.parent_object_id = @P1 \
ORDER BY fk.object_id, k.constraint_column_id"
);

/// The check constraints of table or table type `@P1` (object id).
pub(crate) const CHECKS: &str = concat!(
    "SELECT ck.name, ck.definition, ck.is_disabled, ck.is_not_trusted, ck.is_not_for_replication, ",
    description!("1", "ck.object_id"),
    " AS comment FROM sys.check_constraints ck WHERE ck.parent_object_id = @P1"
);

/// The DML triggers of table or view `@P1` (object id). Database-level DDL triggers have
/// `parent_class = 0` and are never listed.
pub(crate) const TRIGGERS: &str = concat!(
    "SELECT tr.object_id, tr.name, tr.is_ms_shipped, tr.is_disabled, tr.is_instead_of_trigger, \
       tr.is_not_for_replication, m.definition, ",
    execute_as!(),
    " AS execute_as, \
       CAST(CASE WHEN EXISTS (SELECT 1 FROM sys.trigger_events e \
                              WHERE e.object_id = tr.object_id AND e.type_desc = N'INSERT') \
            THEN 1 ELSE 0 END AS bit) AS on_insert, \
       CAST(CASE WHEN EXISTS (SELECT 1 FROM sys.trigger_events e \
                              WHERE e.object_id = tr.object_id AND e.type_desc = N'UPDATE') \
            THEN 1 ELSE 0 END AS bit) AS on_update, \
       CAST(CASE WHEN EXISTS (SELECT 1 FROM sys.trigger_events e \
                              WHERE e.object_id = tr.object_id AND e.type_desc = N'DELETE') \
            THEN 1 ELSE 0 END AS bit) AS on_delete, ",
    description!("1", "tr.object_id"),
    " AS comment \
FROM sys.triggers tr \
LEFT JOIN sys.sql_modules m ON m.object_id = tr.object_id \
LEFT JOIN sys.assembly_modules am ON am.object_id = tr.object_id \
WHERE tr.parent_class = 1 AND tr.parent_id = @P1"
);

/// The sequences of schema `@P1`.
pub(crate) const SEQUENCES: &str = concat!(
    "SELECT q.name, q.is_ms_shipped, ",
    description!("1", "q.object_id"),
    " AS comment FROM sys.sequences q JOIN sys.schemas s ON s.schema_id = q.schema_id \
WHERE s.name = @P1"
);

/// One sequence by schema (`@P1`) and name (`@P2`). Bounds are `sql_variant`s of the
/// sequence's type, read as text.
pub(crate) const SEQUENCE: &str = concat!(
    "SELECT q.name, CAST(q.start_value AS nvarchar(60)) AS start_value, \
       CAST(q.increment AS nvarchar(60)) AS increment, \
       CAST(q.minimum_value AS nvarchar(60)) AS minimum_value, \
       CAST(q.maximum_value AS nvarchar(60)) AS maximum_value, \
       q.is_cycling, q.is_cached, q.cache_size, ",
    type_columns!(),
    ", CAST(t.max_length AS int) AS max_length, CAST(q.precision AS int) AS num_precision, \
       CAST(q.scale AS int) AS scale, ",
    description!("1", "q.object_id"),
    " AS comment FROM sys.sequences q JOIN sys.schemas s ON s.schema_id = q.schema_id",
    type_joins!("q"),
    " WHERE s.name = @P1 AND q.name = @P2"
);

/// The user-defined types (alias, table and CLR) of schema `@P1`.
pub(crate) const TYPES: &str = concat!(
    "SELECT t.name, t.is_table_type, t.is_assembly_type, ",
    description!("6", "t.user_type_id"),
    " AS comment FROM sys.types t JOIN sys.schemas s ON s.schema_id = t.schema_id \
WHERE t.is_user_defined = 1 AND s.name = @P1"
);

/// One user-defined type by schema (`@P1`) and name (`@P2`).
pub(crate) const TYPE: &str = concat!(
    "SELECT t.name, t.is_table_type, t.is_assembly_type, t.is_nullable, bt.name AS base_type, \
       CAST(t.max_length AS int) AS max_length, CAST(t.precision AS int) AS num_precision, \
       CAST(t.scale AS int) AS scale, tt.type_table_object_id, ",
    description!("6", "t.user_type_id"),
    " AS comment \
FROM sys.types t JOIN sys.schemas s ON s.schema_id = t.schema_id \
LEFT JOIN sys.types bt ON bt.user_type_id = t.system_type_id \
                      AND bt.is_user_defined = 0 AND bt.user_type_id <> 256 \
LEFT JOIN sys.table_types tt ON tt.user_type_id = t.user_type_id \
WHERE t.is_user_defined = 1 AND s.name = @P1 AND t.name = @P2"
);

/// Every query, for the text checks below.
#[cfg(test)]
pub(crate) fn all() -> Vec<&'static str> {
    vec![
        DATABASES,
        DATABASE,
        SCHEMAS,
        SCHEMA,
        OBJECT,
        TABLES_2017,
        TABLES_2022,
        TABLE_HEAD_2017,
        TABLE_HEAD_2022,
        VIEWS,
        VIEW,
        FUNCTIONS,
        PROCEDURES,
        ROUTINE,
        PARAMETERS,
        COLUMNS,
        INDEXES,
        INDEX_COLUMNS,
        FOREIGN_KEYS,
        CHECKS,
        TRIGGERS,
        SEQUENCES,
        SEQUENCE,
        TYPES,
        TYPE,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The highest `@Pn` a query uses, and whether `@P1..@Pn` are all used.
    fn placeholders(sql: &str) -> Vec<u32> {
        let mut out = Vec::new();
        let bytes = sql.as_bytes();
        let mut i = 0;
        while i + 2 < bytes.len() {
            if bytes[i] == b'@' && bytes[i + 1] == b'P' {
                let digits: String = sql[i + 2..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                if let Ok(n) = digits.parse() {
                    out.push(n);
                }
            }
            i += 1;
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    #[test]
    fn queries_bind_names_and_never_format_them() {
        for sql in all() {
            // One statement, no batch separators or formatting leftovers.
            assert!(!sql.contains(';'), "{sql}");
            assert!(!sql.contains('{'), "{sql}");
            assert!(!sql.contains('"'), "{sql}");
            // No three-part names, no USE: everything is the session database's catalog.
            assert!(!sql.to_ascii_uppercase().contains("USE "), "{sql}");
            assert!(!sql.contains("].sys."), "{sql}");
            assert!(!sql.contains("INFORMATION_SCHEMA"), "{sql}");
            // Parameters are numbered from 1 without gaps.
            let p = placeholders(sql);
            let expected: Vec<u32> = (1..=u32::try_from(p.len()).unwrap()).collect();
            assert_eq!(p, expected, "{sql}");
            // String literals are fixed catalog values, never names: every quote in the text
            // belongs to one of the fixed literals.
            let fixed = [
                "N'MS_Description'",
                "'Collation'",
                "N'OWNER'",
                "N'INSERT'",
                "N'UPDATE'",
                "N'DELETE'",
                "'FN'",
                "'IF'",
                "'TF'",
                "'FS'",
                "'FT'",
                "'AF'",
                "'P'",
                "'PC'",
            ];
            let mut rest = sql.to_owned();
            for lit in fixed {
                rest = rest.replace(lit, "");
            }
            assert!(!rest.contains('\''), "unexpected literal in {sql}");
        }
    }

    #[test]
    fn name_lookups_take_schema_then_name() {
        for sql in [OBJECT, VIEW, ROUTINE, SEQUENCE, TYPE] {
            assert!(sql.contains("s.name = @P1"), "{sql}");
            assert!(sql.contains(".name = @P2"), "{sql}");
        }
        for sql in [
            VIEWS,
            FUNCTIONS,
            PROCEDURES,
            SEQUENCES,
            TYPES,
            TABLES_2017,
            SCHEMA,
        ] {
            assert!(sql.contains("s.name = @P1"), "{sql}");
        }
        assert!(DATABASE.contains("d.name = @P1"));
    }

    #[test]
    fn versions_pick_matching_queries() {
        let v2017 = ServerVersion::new(14, 0, 3456);
        let v2022 = ServerVersion::new(16, 0, 4000);
        let unknown = ServerVersion::new(0, 0, 0);
        assert!(!tables(&v2017).contains("t.ledger_type"));
        assert!(tables(&v2022).contains("t.ledger_type"));
        assert!(tables(&unknown).contains("t.ledger_type"));
        assert!(!table_head(&v2017).contains("t.ledger_type"));
        assert!(table_head(&ServerVersion::new(17, 0, 0)).contains("t.ledger_type"));
        assert!(at_least(&unknown, 16));
        assert!(!at_least(&v2017, 15));
    }

    #[test]
    fn ddl_triggers_are_never_read() {
        assert!(TRIGGERS.contains("tr.parent_class = 1"));
        assert!(!TRIGGERS.contains("server_triggers"));
    }
}
