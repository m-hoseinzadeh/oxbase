//! Catalog queries of the PostgreSQL fetchers.
//!
//! Every query is a `&'static str` built at compile time: names from object references are only
//! ever bound as parameters (`$1`, `$2`), never written into SQL text (M2 fetcher rule). Version
//! differences are separate constants, picked by the functions at the bottom of this module from
//! the server version.
//!
//! Tables are schema-qualified (`pg_catalog.pg_class`) so a session `search_path` cannot change
//! what is read. Columns of PostgreSQL-internal types (`name`, `"char"`, `oid`) are cast to
//! `text` / `int8` so the driver returns plain `Utf8` / `Int64` columns.
//!
//! Supported servers: PostgreSQL 10 and newer. Version-dependent pieces: `prokind` (11+,
//! procedures), `indnkeyatts` (11+, `INCLUDE` columns), `attgenerated` (12+), `prosqlbody`
//! (14+, SQL-standard bodies), `indnullsnotdistinct` (15+).

use oxbase_types::ServerVersion;

/// Databases a client can connect to (templates and `datallowconn = false` are left out).
pub(crate) const DATABASES: &str = "\
SELECT d.datname::text AS name, \
       shobj_description(d.oid, 'pg_database') AS comment, \
       pg_encoding_to_char(d.encoding)::text AS encoding, \
       d.datcollate::text AS collate_name, \
       (d.datname = current_database()) AS is_current \
FROM pg_catalog.pg_database d \
WHERE NOT d.datistemplate AND d.datallowconn \
ORDER BY d.datname";

/// One database by name (`$1`).
pub(crate) const DATABASE: &str = "\
SELECT d.datname::text AS name, \
       pg_get_userbyid(d.datdba)::text AS owner, \
       shobj_description(d.oid, 'pg_database') AS comment, \
       pg_encoding_to_char(d.encoding)::text AS encoding, \
       d.datcollate::text AS collate_name, \
       d.datctype::text AS ctype, \
       ts.spcname::text AS tablespace, \
       (d.datname = current_database()) AS is_current \
FROM pg_catalog.pg_database d \
LEFT JOIN pg_catalog.pg_tablespace ts ON ts.oid = d.dattablespace \
WHERE d.datname = $1";

/// Schemas of the session's database. Other sessions' temporary schemas are left out; this
/// session's own temporary schema is listed.
pub(crate) const SCHEMAS: &str = "\
SELECT n.nspname::text AS name, \
       obj_description(n.oid, 'pg_namespace') AS comment, \
       (n.oid = pg_my_temp_schema()) AS is_temp \
FROM pg_catalog.pg_namespace n \
WHERE n.nspname !~ '^pg_(toast_)?temp_' OR n.oid = pg_my_temp_schema() \
ORDER BY n.nspname";

/// One schema by name (`$1`).
pub(crate) const SCHEMA: &str = "\
SELECT n.nspname::text AS name, \
       pg_get_userbyid(n.nspowner)::text AS owner, \
       obj_description(n.oid, 'pg_namespace') AS comment, \
       (n.oid = pg_my_temp_schema()) AS is_temp \
FROM pg_catalog.pg_namespace n \
WHERE n.nspname = $1";

/// Relations of the given `relkind`s in schema `$1`.
macro_rules! relations {
    ($kinds:literal) => {
        concat!(
            "SELECT c.relname::text AS name, ",
            "obj_description(c.oid, 'pg_class') AS comment, ",
            "CASE WHEN c.reltuples >= 0 THEN c.reltuples::int8 END AS row_estimate, ",
            "c.relkind::text AS relkind, ",
            "c.relispartition AS is_partition, ",
            "c.relpersistence::text AS persistence, ",
            "c.relispopulated AS populated ",
            "FROM pg_catalog.pg_class c ",
            "JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace ",
            "WHERE n.nspname = $1 AND c.relkind IN (",
            $kinds,
            ") ORDER BY c.relname"
        )
    };
}

/// Tables of schema `$1`: ordinary, partitioned and foreign.
pub(crate) const TABLES: &str = relations!("'r', 'p', 'f'");
/// Views of schema `$1`.
pub(crate) const VIEWS: &str = relations!("'v'");
/// Materialized views of schema `$1`.
pub(crate) const MATVIEWS: &str = relations!("'m'");
/// Sequences of schema `$1`.
pub(crate) const SEQUENCES: &str = relations!("'S'");

/// One relation by schema (`$1`) and name (`$2`): its oid and `relkind`.
pub(crate) const RELATION: &str = "\
SELECT c.oid::int8 AS oid, c.relkind::text AS relkind \
FROM pg_catalog.pg_class c \
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
WHERE n.nspname = $1 AND c.relname = $2";

/// Table-level facts of relation `$1` (oid).
pub(crate) const TABLE_HEAD: &str = "\
SELECT obj_description(c.oid, 'pg_class') AS comment, \
       c.relkind::text AS relkind, \
       c.relpersistence::text AS persistence, \
       c.relispartition AS is_partition, \
       pg_get_userbyid(c.relowner)::text AS owner, \
       ts.spcname::text AS tablespace, \
       pg_get_partkeydef(c.oid) AS partition_by \
FROM pg_catalog.pg_class c \
LEFT JOIN pg_catalog.pg_tablespace ts ON ts.oid = c.reltablespace \
WHERE c.oid = $1";

/// Columns of relation `$1` (oid), in ordinal order. `$generated` is the `attgenerated`
/// expression (12+) or an empty string.
macro_rules! columns {
    ($generated:literal) => {
        concat!(
            "SELECT a.attnum::int4 AS ordinal, ",
            "a.attname::text AS name, ",
            "format_type(a.atttypid, a.atttypmod) AS native_type, ",
            "NOT a.attnotnull AS nullable, ",
            "pg_get_expr(ad.adbin, ad.adrelid, true) AS default_expr, ",
            "a.attidentity::text AS identity, ",
            $generated,
            " AS generated, ",
            "CASE WHEN a.attcollation <> t.typcollation THEN co.collname::text END ",
            "AS collation_name, ",
            "col_description(a.attrelid, a.attnum) AS comment, ",
            "EXISTS (SELECT 1 FROM pg_catalog.pg_constraint k WHERE k.conrelid = a.attrelid ",
            "AND k.contype = 'p' AND a.attnum = ANY (k.conkey)) AS in_primary_key, ",
            "EXISTS (SELECT 1 FROM pg_catalog.pg_depend dep ",
            "JOIN pg_catalog.pg_class sq ON sq.oid = dep.objid AND sq.relkind = 'S' ",
            "WHERE dep.classid = 'pg_catalog.pg_class'::regclass ",
            "AND dep.refclassid = 'pg_catalog.pg_class'::regclass ",
            "AND dep.refobjid = a.attrelid AND dep.refobjsubid = a.attnum ",
            "AND dep.deptype = 'a') AS owns_sequence, ",
            "idseq.seqstart AS identity_start, ",
            "idseq.seqincrement AS identity_increment ",
            "FROM pg_catalog.pg_attribute a ",
            "JOIN pg_catalog.pg_type t ON t.oid = a.atttypid ",
            "LEFT JOIN pg_catalog.pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum ",
            "LEFT JOIN pg_catalog.pg_collation co ON co.oid = a.attcollation ",
            "LEFT JOIN LATERAL (SELECT s.seqstart, s.seqincrement ",
            "FROM pg_catalog.pg_depend dep ",
            "JOIN pg_catalog.pg_sequence s ON s.seqrelid = dep.objid ",
            "WHERE dep.classid = 'pg_catalog.pg_class'::regclass ",
            "AND dep.refclassid = 'pg_catalog.pg_class'::regclass ",
            "AND dep.refobjid = a.attrelid AND dep.refobjsubid = a.attnum ",
            "AND dep.deptype = 'i' LIMIT 1) idseq ON true ",
            "WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped ",
            "ORDER BY a.attnum"
        )
    };
}

/// [`columns`] for PostgreSQL 12+ (generated columns).
pub(crate) const COLUMNS_12: &str = columns!("a.attgenerated::text");
/// [`columns`] before PostgreSQL 12.
pub(crate) const COLUMNS_10: &str = columns!("''::text");

/// Primary key, unique, foreign key and check constraints of relation `$1` (oid).
pub(crate) const CONSTRAINTS: &str = "\
SELECT k.oid::int8 AS oid, \
       k.conname::text AS name, \
       k.contype::text AS contype, \
       k.condeferrable AS is_deferrable, \
       k.condeferred AS is_deferred, \
       k.convalidated AS is_validated, \
       k.connoinherit AS no_inherit, \
       CASE WHEN k.contype = 'c' THEN pg_get_expr(k.conbin, k.conrelid, true) END AS check_expr, \
       k.confupdtype::text AS on_update, \
       k.confdeltype::text AS on_delete, \
       k.confmatchtype::text AS match_type, \
       rn.nspname::text AS ref_schema, \
       rc.relname::text AS ref_table, \
       obj_description(k.oid, 'pg_constraint') AS comment \
FROM pg_catalog.pg_constraint k \
LEFT JOIN pg_catalog.pg_class rc ON rc.oid = k.confrelid \
LEFT JOIN pg_catalog.pg_namespace rn ON rn.oid = rc.relnamespace \
WHERE k.conrelid = $1 AND k.contype IN ('p', 'u', 'f', 'c') \
ORDER BY k.conname";

/// Key columns (`is_referenced = false`) and referenced columns (`true`, foreign keys) of the
/// constraints of relation `$1` (oid), in key order.
pub(crate) const CONSTRAINT_COLUMNS: &str = "\
SELECT k.oid::int8 AS con, false AS is_referenced, x.ord::int4 AS ord, a.attname::text AS name \
FROM pg_catalog.pg_constraint k \
CROSS JOIN LATERAL unnest(k.conkey) WITH ORDINALITY AS x(attnum, ord) \
JOIN pg_catalog.pg_attribute a ON a.attrelid = k.conrelid AND a.attnum = x.attnum \
WHERE k.conrelid = $1 AND k.contype IN ('p', 'u', 'f', 'c') \
UNION ALL \
SELECT k.oid::int8, true, x.ord::int4, a.attname::text \
FROM pg_catalog.pg_constraint k \
CROSS JOIN LATERAL unnest(k.confkey) WITH ORDINALITY AS x(attnum, ord) \
JOIN pg_catalog.pg_attribute a ON a.attrelid = k.confrelid AND a.attnum = x.attnum \
WHERE k.conrelid = $1 AND k.contype = 'f' \
ORDER BY 1, 2, 3";

/// Indexes of relation `$1` (oid). `$keys` counts the key columns (`indnkeyatts`, 11+, else
/// `indnatts`); `$nnd` is `indnullsnotdistinct` (15+) or `false`.
macro_rules! indexes {
    ($keys:literal, $nnd:literal) => {
        concat!(
            "SELECT ci.oid::int8 AS oid, ",
            "ci.relname::text AS name, ",
            "i.indisunique AS is_unique, ",
            "i.indisprimary AS is_primary, ",
            "i.indisvalid AS is_valid, ",
            "am.amname::text AS method, ",
            "pg_get_expr(i.indpred, i.indrelid, true) AS predicate, ",
            $keys,
            "::int4 AS key_count, ",
            $nnd,
            " AS nulls_not_distinct, ",
            "EXISTS (SELECT 1 FROM pg_catalog.pg_constraint k WHERE k.conindid = ci.oid ",
            "AND k.conrelid = i.indrelid AND k.contype = 'u') AS is_constraint, ",
            "obj_description(ci.oid, 'pg_class') AS comment ",
            "FROM pg_catalog.pg_index i ",
            "JOIN pg_catalog.pg_class ci ON ci.oid = i.indexrelid ",
            "JOIN pg_catalog.pg_am am ON am.oid = ci.relam ",
            "WHERE i.indrelid = $1 ",
            "ORDER BY ci.relname"
        )
    };
}

/// [`indexes`] for PostgreSQL 15+.
pub(crate) const INDEXES_15: &str = indexes!("i.indnkeyatts", "i.indnullsnotdistinct");
/// [`indexes`] for PostgreSQL 11 to 14.
pub(crate) const INDEXES_11: &str = indexes!("i.indnkeyatts", "false");
/// [`indexes`] before PostgreSQL 11 (no `INCLUDE` columns).
pub(crate) const INDEXES_10: &str = indexes!("i.indnatts", "false");

/// Every column of every index of relation `$1` (oid), key columns first, then `INCLUDE`
/// columns. `name` is NULL for an expression; `expr` is the column or expression text.
pub(crate) const INDEX_COLUMNS: &str = "\
SELECT i.indexrelid::int8 AS idx, \
       k.n AS ord, \
       a.attname::text AS name, \
       pg_get_indexdef(i.indexrelid, k.n, true) AS expr, \
       COALESCE((i.indoption[k.n - 1]::int4 & 1) = 1, false) AS descending \
FROM pg_catalog.pg_index i \
CROSS JOIN LATERAL generate_series(1, i.indnatts::int4) AS k(n) \
LEFT JOIN pg_catalog.pg_attribute a \
       ON a.attrelid = i.indrelid AND a.attnum = i.indkey[k.n - 1] AND i.indkey[k.n - 1] <> 0 \
WHERE i.indrelid = $1 \
ORDER BY 1, 2";

/// User triggers of relation `$1` (oid). `condition` is the `WHEN (...)` text, taken from the
/// server's own trigger definition.
pub(crate) const TRIGGERS: &str = "\
SELECT t.oid::int8 AS oid, \
       t.tgname::text AS name, \
       t.tgtype::int4 AS tgtype, \
       t.tgenabled::text AS enabled, \
       pg_get_triggerdef(t.oid, true) AS definition, \
       (regexp_match(pg_get_triggerdef(t.oid), \
                     '.{35,} WHEN \\((.+)\\) EXECUTE (?:FUNCTION|PROCEDURE)'))[1] AS condition, \
       fn.nspname::text AS fn_schema, \
       p.proname::text AS fn_name, \
       pg_get_function_identity_arguments(p.oid) AS fn_signature, \
       obj_description(t.oid, 'pg_trigger') AS comment \
FROM pg_catalog.pg_trigger t \
JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid \
JOIN pg_catalog.pg_namespace fn ON fn.oid = p.pronamespace \
WHERE t.tgrelid = $1 AND NOT t.tgisinternal \
ORDER BY t.tgname";

/// `UPDATE OF` columns of trigger `$1` (oid), in declaration order.
pub(crate) const TRIGGER_COLUMNS: &str = "\
SELECT a.attname::text AS name \
FROM pg_catalog.pg_trigger t \
CROSS JOIN LATERAL unnest(t.tgattr::int2[]) WITH ORDINALITY AS k(attnum, ord) \
JOIN pg_catalog.pg_attribute a ON a.attrelid = t.tgrelid AND a.attnum = k.attnum \
WHERE t.oid = $1 \
ORDER BY k.ord";

/// Routines of schema `$1` matching `$filter`; with `$by_name`, only those named `$2`.
macro_rules! routines {
    ($filter:literal, $by_name:literal) => {
        concat!(
            "SELECT p.oid::int8 AS oid, ",
            "p.proname::text AS name, ",
            "pg_get_function_identity_arguments(p.oid) AS signature, ",
            "obj_description(p.oid, 'pg_proc') AS comment ",
            "FROM pg_catalog.pg_proc p ",
            "JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace ",
            "WHERE n.nspname = $1 AND ",
            $filter,
            $by_name,
            " ORDER BY p.proname, 3"
        )
    };
}

/// Functions (plain and window; not aggregates or procedures) of schema `$1`, PostgreSQL 11+.
pub(crate) const FUNCTIONS_11: &str = routines!("p.prokind IN ('f', 'w')", "");
/// Functions of schema `$1` named `$2`, PostgreSQL 11+.
pub(crate) const FUNCTIONS_BY_NAME_11: &str =
    routines!("p.prokind IN ('f', 'w')", " AND p.proname = $2");
/// Functions of schema `$1` before PostgreSQL 11.
pub(crate) const FUNCTIONS_10: &str = routines!("NOT p.proisagg", "");
/// Functions of schema `$1` named `$2` before PostgreSQL 11.
pub(crate) const FUNCTIONS_BY_NAME_10: &str = routines!("NOT p.proisagg", " AND p.proname = $2");
/// Procedures of schema `$1` (PostgreSQL 11+).
pub(crate) const PROCEDURES: &str = routines!("p.prokind = 'p'", "");
/// Procedures of schema `$1` named `$2` (PostgreSQL 11+).
pub(crate) const PROCEDURES_BY_NAME: &str = routines!("p.prokind = 'p'", " AND p.proname = $2");

/// Details of routine `$1` (oid). `$sql_body` is the SQL-standard body (14+) or NULL.
macro_rules! routine {
    ($sql_body:literal) => {
        concat!(
            "SELECT pg_get_function_result(p.oid) AS returns, ",
            "l.lanname::text AS language, ",
            "p.prosrc AS src, ",
            $sql_body,
            " AS sql_body, ",
            "pg_get_functiondef(p.oid) AS definition, ",
            "obj_description(p.oid, 'pg_proc') AS comment, ",
            "p.provolatile::text AS volatility, ",
            "p.proisstrict AS is_strict, ",
            "p.prosecdef AS security_definer, ",
            "p.proparallel::text AS parallel, ",
            "pg_get_userbyid(p.proowner)::text AS owner ",
            "FROM pg_catalog.pg_proc p ",
            "JOIN pg_catalog.pg_language l ON l.oid = p.prolang ",
            "WHERE p.oid = $1"
        )
    };
}

/// [`routine`] for PostgreSQL 14+.
pub(crate) const ROUTINE_14: &str =
    routine!("CASE WHEN p.prosqlbody IS NOT NULL THEN pg_get_function_sqlbody(p.oid) END");
/// [`routine`] before PostgreSQL 14.
pub(crate) const ROUTINE_10: &str = routine!("NULL::text");

/// Arguments of routine `$1` (oid) in declaration order, including `OUT` and `TABLE` columns.
pub(crate) const ROUTINE_ARGS: &str = "\
SELECT a.n AS ord, \
       (p.proargnames)[a.n] AS name, \
       COALESCE((p.proargmodes)[a.n]::text, 'i') AS mode, \
       format_type(COALESCE((p.proallargtypes)[a.n], p.proargtypes[a.n - 1]), NULL) AS native_type, \
       pg_get_function_arg_default(p.oid, a.n) AS default_expr \
FROM pg_catalog.pg_proc p \
CROSS JOIN LATERAL generate_series(1, COALESCE(array_length(p.proallargtypes, 1), p.pronargs::int4)) AS a(n) \
WHERE p.oid = $1 \
ORDER BY a.n";

/// One sequence by schema (`$1`) and name (`$2`), with the column that owns it.
pub(crate) const SEQUENCE: &str = "\
SELECT format_type(s.seqtypid, NULL) AS data_type, \
       s.seqstart AS start_value, \
       s.seqincrement AS increment_by, \
       s.seqmin AS min_value, \
       s.seqmax AS max_value, \
       s.seqcache AS cache_size, \
       s.seqcycle AS is_cycle, \
       obj_description(c.oid, 'pg_class') AS comment, \
       pg_get_userbyid(c.relowner)::text AS owner, \
       dep.deptype::text AS dep_type, \
       own_n.nspname::text AS owned_schema, \
       own_c.relname::text AS owned_table, \
       own_a.attname::text AS owned_column \
FROM pg_catalog.pg_class c \
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
JOIN pg_catalog.pg_sequence s ON s.seqrelid = c.oid \
LEFT JOIN pg_catalog.pg_depend dep \
       ON dep.classid = 'pg_catalog.pg_class'::regclass AND dep.objid = c.oid \
      AND dep.refclassid = 'pg_catalog.pg_class'::regclass AND dep.refobjsubid > 0 \
      AND dep.deptype IN ('a', 'i') \
LEFT JOIN pg_catalog.pg_class own_c ON own_c.oid = dep.refobjid \
LEFT JOIN pg_catalog.pg_namespace own_n ON own_n.oid = own_c.relnamespace \
LEFT JOIN pg_catalog.pg_attribute own_a \
       ON own_a.attrelid = dep.refobjid AND own_a.attnum = dep.refobjsubid \
WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'S'";

/// A view or materialized view by schema (`$1`) and name (`$2`).
pub(crate) const VIEW: &str = "\
SELECT c.relkind::text AS relkind, \
       pg_get_viewdef(c.oid, true) AS definition, \
       obj_description(c.oid, 'pg_class') AS comment, \
       pg_get_userbyid(c.relowner)::text AS owner, \
       c.relispopulated AS populated, \
       ts.spcname::text AS tablespace, \
       (SELECT o.option_value FROM pg_options_to_table(c.reloptions) o \
         WHERE o.option_name = 'check_option') AS check_option \
FROM pg_catalog.pg_class c \
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
LEFT JOIN pg_catalog.pg_tablespace ts ON ts.oid = c.reltablespace \
WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('v', 'm')";

/// User-defined types of schema `$1`: enums, standalone composites, domains, ranges and
/// multiranges (array types and table row types are left out).
pub(crate) const TYPES: &str = "\
SELECT t.typname::text AS name, \
       obj_description(t.oid, 'pg_type') AS comment, \
       t.typtype::text AS typtype \
FROM pg_catalog.pg_type t \
JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
WHERE n.nspname = $1 AND t.typtype IN ('c', 'd', 'e', 'r', 'm') \
  AND (t.typtype <> 'c' OR EXISTS (SELECT 1 FROM pg_catalog.pg_class c \
                                    WHERE c.oid = t.typrelid AND c.relkind = 'c')) \
ORDER BY t.typname";

/// One type by schema (`$1`) and name (`$2`).
pub(crate) const TYPE: &str = "\
SELECT t.typtype::text AS typtype, \
       obj_description(t.oid, 'pg_type') AS comment, \
       pg_get_userbyid(t.typowner)::text AS owner, \
       CASE WHEN t.typtype = 'd' THEN format_type(t.typbasetype, t.typtypmod) END AS base_type, \
       (SELECT string_agg(quote_literal(e.enumlabel), ', ' ORDER BY e.enumsortorder) \
          FROM pg_catalog.pg_enum e WHERE e.enumtypid = t.oid) AS enum_labels \
FROM pg_catalog.pg_type t \
JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
WHERE n.nspname = $1 AND t.typname = $2";

/// Extensions installed in the session's database.
pub(crate) const EXTENSIONS: &str = "\
SELECT e.extname::text AS name, \
       obj_description(e.oid, 'pg_extension') AS comment, \
       e.extversion AS version, \
       n.nspname::text AS schema_name \
FROM pg_catalog.pg_extension e \
JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace \
ORDER BY e.extname";

/// One extension by name (`$1`).
pub(crate) const EXTENSION: &str = "\
SELECT e.extname::text AS name, \
       obj_description(e.oid, 'pg_extension') AS comment, \
       e.extversion AS version, \
       n.nspname::text AS schema_name, \
       pg_get_userbyid(e.extowner)::text AS owner \
FROM pg_catalog.pg_extension e \
JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace \
WHERE e.extname = $1";

/// `true` when `v` is at least `major`. An unknown version (`0.0.0`, the driver's fallback when
/// the server reported none) counts as current.
pub(crate) fn at_least(v: &ServerVersion, major: u32) -> bool {
    v.major == 0 || v.at_least(major, 0)
}

/// The column query for `v`.
pub(crate) fn columns(v: &ServerVersion) -> &'static str {
    if at_least(v, 12) {
        COLUMNS_12
    } else {
        COLUMNS_10
    }
}

/// The index query for `v`.
pub(crate) fn indexes(v: &ServerVersion) -> &'static str {
    if at_least(v, 15) {
        INDEXES_15
    } else if at_least(v, 11) {
        INDEXES_11
    } else {
        INDEXES_10
    }
}

/// The function list query for `v` (`by_name`: also filtered by `$2`).
pub(crate) fn functions(v: &ServerVersion, by_name: bool) -> &'static str {
    match (at_least(v, 11), by_name) {
        (true, false) => FUNCTIONS_11,
        (true, true) => FUNCTIONS_BY_NAME_11,
        (false, false) => FUNCTIONS_10,
        (false, true) => FUNCTIONS_BY_NAME_10,
    }
}

/// The procedure list query (`by_name`: also filtered by `$2`). Procedures exist from 11.
pub(crate) fn procedures(by_name: bool) -> &'static str {
    if by_name {
        PROCEDURES_BY_NAME
    } else {
        PROCEDURES
    }
}

/// The routine detail query for `v`.
pub(crate) fn routine(v: &ServerVersion) -> &'static str {
    if at_least(v, 14) {
        ROUTINE_14
    } else {
        ROUTINE_10
    }
}

/// Every query of this module (for the static checks in the tests).
#[cfg(test)]
pub(crate) const ALL: &[&str] = &[
    DATABASES,
    DATABASE,
    SCHEMAS,
    SCHEMA,
    TABLES,
    VIEWS,
    MATVIEWS,
    SEQUENCES,
    RELATION,
    TABLE_HEAD,
    COLUMNS_12,
    COLUMNS_10,
    CONSTRAINTS,
    CONSTRAINT_COLUMNS,
    INDEXES_15,
    INDEXES_11,
    INDEXES_10,
    INDEX_COLUMNS,
    TRIGGERS,
    TRIGGER_COLUMNS,
    FUNCTIONS_11,
    FUNCTIONS_BY_NAME_11,
    FUNCTIONS_10,
    FUNCTIONS_BY_NAME_10,
    PROCEDURES,
    PROCEDURES_BY_NAME,
    ROUTINE_14,
    ROUTINE_10,
    ROUTINE_ARGS,
    SEQUENCE,
    VIEW,
    TYPES,
    TYPE,
    EXTENSIONS,
    EXTENSION,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The `$n` placeholders of `sql`, sorted and deduplicated.
    fn placeholders(sql: &str) -> Vec<u32> {
        let bytes = sql.as_bytes();
        let mut out = Vec::new();
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'$' {
                let digits: String = sql[i + 1..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                assert_ne!(digits, "", "bare $ in {sql}");
                out.push(digits.parse().unwrap());
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    #[test]
    fn queries_are_single_statements_with_numbered_parameters_only() {
        for sql in ALL {
            assert!(!sql.contains(';'), "statement separator in {sql}");
            assert!(!sql.contains("{}"), "format placeholder in {sql}");
            // Dollar quotes would hide text from the placeholder check.
            assert!(!sql.contains("$$"), "dollar quote in {sql}");
            let params = placeholders(sql);
            let expected: Vec<u32> = (1..=u32::try_from(params.len()).unwrap()).collect();
            assert_eq!(params, expected, "placeholders of {sql}");
            assert!(params.len() <= 2, "{sql}");
        }
    }

    #[test]
    fn catalog_tables_are_qualified() {
        for sql in ALL {
            for keyword in ["FROM ", "JOIN "] {
                for (at, _) in sql.match_indices(keyword) {
                    let rest = &sql[at + keyword.len()..];
                    if !rest.starts_with("pg_") {
                        continue;
                    }
                    let token: String = rest
                        .chars()
                        .take_while(|c| !c.is_whitespace() && *c != ')')
                        .collect();
                    assert!(
                        token.starts_with("pg_catalog.") || token.contains('('),
                        "unqualified catalog table {token} in {sql}"
                    );
                }
            }
        }
    }

    #[test]
    fn versions_pick_matching_queries() {
        let v10 = ServerVersion::new(10, 23, 0);
        let v11 = ServerVersion::new(11, 22, 0);
        let v12 = ServerVersion::new(12, 22, 0);
        let v14 = ServerVersion::new(14, 19, 0);
        let v15 = ServerVersion::new(15, 14, 0);
        let v18 = ServerVersion::new(18, 6, 0);
        let unknown = ServerVersion::new(0, 0, 0);

        assert!(!columns(&v11).contains("attgenerated"));
        assert!(columns(&v12).contains("attgenerated"));
        assert!(columns(&unknown).contains("attgenerated"));

        assert!(!indexes(&v10).contains("indnkeyatts"));
        assert!(indexes(&v11).contains("indnkeyatts"));
        assert!(!indexes(&v14).contains("indnullsnotdistinct"));
        assert!(indexes(&v15).contains("indnullsnotdistinct"));
        assert!(indexes(&v18).contains("indnullsnotdistinct"));

        assert!(functions(&v10, false).contains("proisagg"));
        assert!(functions(&v11, false).contains("prokind"));
        assert!(functions(&v18, true).contains("$2"));
        assert!(!functions(&v18, false).contains("$2"));
        assert!(procedures(true).contains("$2"));

        assert!(!routine(&v12).contains("prosqlbody"));
        assert!(routine(&v14).contains("prosqlbody"));
    }
}
