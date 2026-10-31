//! Catalog queries of the PostgreSQL fetchers.
//!
//! Every query is a `&'static str` built at compile time: names from object references are only
//! ever bound as parameters (`$1`, `$2`), never written into SQL text (M2 fetcher rule). Version
//! differences are separate constants, picked by the functions at the bottom of this module from
//! the server version.
//!
//! Catalog tables and functions are schema-qualified (`pg_catalog.pg_class`,
//! `pg_catalog.format_type(...)`), so objects of the same name in a schema the session
//! `search_path` lists before `pg_catalog` are not picked up instead. Type names in casts and
//! operators are still resolved through the `search_path`, where `pg_catalog` comes first unless
//! the user explicitly lists it later. Columns of PostgreSQL-internal types (`name`, `"char"`,
//! `oid`) are cast to `text` / `int8` so the driver returns plain `Utf8` / `Int64` columns.
//!
//! Supported servers: PostgreSQL 10 and newer. Version-dependent pieces: `prokind` (11+,
//! procedures), `indnkeyatts` and `conparentid` (11+, `INCLUDE` columns, partition constraint
//! clones), `attgenerated` (12+), `prosqlbody` (14+, SQL-standard bodies),
//! `indnullsnotdistinct` (15+), `conenforced` (18+, `NOT ENFORCED` constraints). Only the newest
//! branch runs live in CI (the postgres fixture is PostgreSQL 18); the older branches are checked
//! by the query-text tests below.

use oxbase_types::ServerVersion;

/// Databases a client can connect to (templates and `datallowconn = false` are left out).
pub(crate) const DATABASES: &str = "\
SELECT d.datname::text AS name, \
       pg_catalog.shobj_description(d.oid, 'pg_database') AS comment, \
       pg_catalog.pg_encoding_to_char(d.encoding)::text AS encoding, \
       d.datcollate::text AS collate_name, \
       (d.datname = pg_catalog.current_database()) AS is_current \
FROM pg_catalog.pg_database d \
WHERE NOT d.datistemplate AND d.datallowconn \
ORDER BY d.datname";

/// One database by name (`$1`).
pub(crate) const DATABASE: &str = "\
SELECT d.datname::text AS name, \
       pg_catalog.pg_get_userbyid(d.datdba)::text AS owner, \
       pg_catalog.shobj_description(d.oid, 'pg_database') AS comment, \
       pg_catalog.pg_encoding_to_char(d.encoding)::text AS encoding, \
       d.datcollate::text AS collate_name, \
       d.datctype::text AS ctype, \
       ts.spcname::text AS tablespace, \
       (d.datname = pg_catalog.current_database()) AS is_current \
FROM pg_catalog.pg_database d \
LEFT JOIN pg_catalog.pg_tablespace ts ON ts.oid = d.dattablespace \
WHERE d.datname = $1";

/// Schemas of the session's database. Other sessions' temporary schemas are left out; this
/// session's own temporary schema is listed.
pub(crate) const SCHEMAS: &str = "\
SELECT n.nspname::text AS name, \
       pg_catalog.obj_description(n.oid, 'pg_namespace') AS comment, \
       (n.oid = pg_catalog.pg_my_temp_schema()) AS is_temp \
FROM pg_catalog.pg_namespace n \
WHERE n.nspname !~ '^pg_(toast_)?temp_' OR n.oid = pg_catalog.pg_my_temp_schema() \
ORDER BY n.nspname";

/// One schema by name (`$1`).
pub(crate) const SCHEMA: &str = "\
SELECT n.nspname::text AS name, \
       pg_catalog.pg_get_userbyid(n.nspowner)::text AS owner, \
       pg_catalog.obj_description(n.oid, 'pg_namespace') AS comment, \
       (n.oid = pg_catalog.pg_my_temp_schema()) AS is_temp \
FROM pg_catalog.pg_namespace n \
WHERE n.nspname = $1";

/// Relations of the given `relkind`s in schema `$1`.
macro_rules! relations {
    ($kinds:literal) => {
        concat!(
            "SELECT c.relname::text AS name, ",
            "pg_catalog.obj_description(c.oid, 'pg_class') AS comment, ",
            "CASE WHEN c.reltuples >= 0 THEN c.reltuples::int8 END AS row_estimate, ",
            "c.relkind::text AS relkind, ",
            "c.relispartition AS is_partition, ",
            "c.relpersistence::text AS persistence, ",
            "c.relispopulated AS populated, ",
            "EXISTS (SELECT 1 FROM pg_catalog.pg_depend d ",
            "WHERE d.classid = 'pg_catalog.pg_class'::regclass AND d.objid = c.oid ",
            "AND d.deptype = 'e') AS extension_member ",
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

/// One relation by schema (`$1`) and name (`$2`): its oid, `relkind` and whether an extension
/// owns it.
pub(crate) const RELATION: &str = "\
SELECT c.oid::int8 AS oid, c.relkind::text AS relkind, \
       EXISTS (SELECT 1 FROM pg_catalog.pg_depend d \
                WHERE d.classid = 'pg_catalog.pg_class'::regclass AND d.objid = c.oid \
                  AND d.deptype = 'e') AS extension_member \
FROM pg_catalog.pg_class c \
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
WHERE n.nspname = $1 AND c.relname = $2";

/// Table-level facts of relation `$1` (oid).
pub(crate) const TABLE_HEAD: &str = "\
SELECT pg_catalog.obj_description(c.oid, 'pg_class') AS comment, \
       c.relkind::text AS relkind, \
       c.relpersistence::text AS persistence, \
       c.relispartition AS is_partition, \
       pg_catalog.pg_get_userbyid(c.relowner)::text AS owner, \
       ts.spcname::text AS tablespace, \
       pg_catalog.pg_get_partkeydef(c.oid) AS partition_by \
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
            "pg_catalog.format_type(a.atttypid, a.atttypmod) AS native_type, ",
            "NOT a.attnotnull AS nullable, ",
            "pg_catalog.pg_get_expr(ad.adbin, ad.adrelid, true) AS default_expr, ",
            "a.attidentity::text AS identity, ",
            $generated,
            " AS generated, ",
            "CASE WHEN a.attcollation <> t.typcollation THEN ",
            "CASE WHEN pg_catalog.pg_collation_is_visible(co.oid) THEN co.collname::text ",
            "ELSE '\"' || pg_catalog.replace(cn.nspname::text, '\"', '\"\"') || '\".\"' || ",
            "pg_catalog.replace(co.collname::text, '\"', '\"\"') || '\"' END END ",
            "AS collation_name, ",
            "pg_catalog.col_description(a.attrelid, a.attnum) AS comment, ",
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
            "LEFT JOIN pg_catalog.pg_namespace cn ON cn.oid = co.collnamespace ",
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

/// Primary key, unique, foreign key and check constraints of relation `$1` (oid). `$enforced`
/// is `conenforced` (18+) or `true`; `$clones` leaves out the foreign keys PostgreSQL 12+ adds
/// for each partition of a referenced partitioned table (`conparentid` on the same relation;
/// 11+) or is empty. Inherited foreign keys of a partition (parent constraint on another
/// relation) stay.
macro_rules! constraints {
    ($enforced:literal, $clones:literal) => {
        concat!(
            "SELECT k.oid::int8 AS oid, ",
            "k.conname::text AS name, ",
            "k.contype::text AS contype, ",
            "k.condeferrable AS is_deferrable, ",
            "k.condeferred AS is_deferred, ",
            "k.convalidated AS is_validated, ",
            $enforced,
            " AS is_enforced, ",
            "k.connoinherit AS no_inherit, ",
            "CASE WHEN k.contype = 'c' THEN pg_catalog.pg_get_expr(k.conbin, k.conrelid, true) END ",
            "AS check_expr, ",
            "k.confupdtype::text AS on_update, ",
            "k.confdeltype::text AS on_delete, ",
            "k.confmatchtype::text AS match_type, ",
            "rn.nspname::text AS ref_schema, ",
            "rc.relname::text AS ref_table, ",
            "pg_catalog.obj_description(k.oid, 'pg_constraint') AS comment ",
            "FROM pg_catalog.pg_constraint k ",
            "LEFT JOIN pg_catalog.pg_class rc ON rc.oid = k.confrelid ",
            "LEFT JOIN pg_catalog.pg_namespace rn ON rn.oid = rc.relnamespace ",
            "WHERE k.conrelid = $1 AND k.contype IN ('p', 'u', 'f', 'c')",
            $clones,
            " ORDER BY k.conname"
        )
    };
}

/// [`constraints`] for PostgreSQL 18+.
pub(crate) const CONSTRAINTS_18: &str = constraints!(
    "k.conenforced",
    " AND NOT (k.contype = 'f' AND EXISTS (SELECT 1 FROM pg_catalog.pg_constraint pk \
     WHERE pk.oid = k.conparentid AND pk.conrelid = k.conrelid))"
);
/// [`constraints`] for PostgreSQL 11 to 17.
pub(crate) const CONSTRAINTS_11: &str = constraints!(
    "true",
    " AND NOT (k.contype = 'f' AND EXISTS (SELECT 1 FROM pg_catalog.pg_constraint pk \
     WHERE pk.oid = k.conparentid AND pk.conrelid = k.conrelid))"
);
/// [`constraints`] before PostgreSQL 11 (no `conparentid`; partitioned tables cannot be
/// referenced yet).
pub(crate) const CONSTRAINTS_10: &str = constraints!("true", "");

/// Key columns (`is_referenced = false`) and referenced columns (`true`, foreign keys) of the
/// constraints of relation `$1` (oid), in key order. Rows of constraints [`constraints`] leaves
/// out (partition clones) are ignored by the caller.
pub(crate) const CONSTRAINT_COLUMNS: &str = "\
SELECT k.oid::int8 AS con, false AS is_referenced, x.ord::int4 AS ord, a.attname::text AS name \
FROM pg_catalog.pg_constraint k \
CROSS JOIN LATERAL pg_catalog.unnest(k.conkey) WITH ORDINALITY AS x(attnum, ord) \
JOIN pg_catalog.pg_attribute a ON a.attrelid = k.conrelid AND a.attnum = x.attnum \
WHERE k.conrelid = $1 AND k.contype IN ('p', 'u', 'f', 'c') \
UNION ALL \
SELECT k.oid::int8, true, x.ord::int4, a.attname::text \
FROM pg_catalog.pg_constraint k \
CROSS JOIN LATERAL pg_catalog.unnest(k.confkey) WITH ORDINALITY AS x(attnum, ord) \
JOIN pg_catalog.pg_attribute a ON a.attrelid = k.confrelid AND a.attnum = x.attnum \
WHERE k.conrelid = $1 AND k.contype = 'f' \
ORDER BY 1, 2, 3";

/// Indexes of relation `$1` (oid), with whether a `UNIQUE` or `EXCLUDE` constraint owns each
/// (and the `EXCLUDE` definition). `$keys` counts the key columns (`indnkeyatts`, 11+, else
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
            "pg_catalog.pg_get_expr(i.indpred, i.indrelid, true) AS predicate, ",
            $keys,
            "::int4 AS key_count, ",
            $nnd,
            " AS nulls_not_distinct, ",
            "EXISTS (SELECT 1 FROM pg_catalog.pg_constraint k WHERE k.conindid = ci.oid ",
            "AND k.conrelid = i.indrelid AND k.contype IN ('u', 'x')) AS is_constraint, ",
            "(SELECT pg_catalog.pg_get_constraintdef(k.oid, true) FROM pg_catalog.pg_constraint k ",
            "WHERE k.conindid = ci.oid AND k.conrelid = i.indrelid AND k.contype = 'x' ",
            "LIMIT 1) AS exclusion, ",
            "COALESCE((SELECT k.condeferrable FROM pg_catalog.pg_constraint k ",
            "WHERE k.conindid = ci.oid AND k.conrelid = i.indrelid AND k.contype IN ('u', 'x') ",
            "LIMIT 1), false) AS con_deferrable, ",
            "COALESCE((SELECT k.condeferred FROM pg_catalog.pg_constraint k ",
            "WHERE k.conindid = ci.oid AND k.conrelid = i.indrelid AND k.contype IN ('u', 'x') ",
            "LIMIT 1), false) AS con_deferred, ",
            "pg_catalog.array_to_json(ci.reloptions)::text AS reloptions, ",
            "its.spcname::text AS tablespace, ",
            "pg_catalog.obj_description(ci.oid, 'pg_class') AS comment ",
            "FROM pg_catalog.pg_index i ",
            "JOIN pg_catalog.pg_class ci ON ci.oid = i.indexrelid ",
            "JOIN pg_catalog.pg_am am ON am.oid = ci.relam ",
            "LEFT JOIN pg_catalog.pg_tablespace its ON its.oid = ci.reltablespace ",
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
/// columns. `name` is NULL for an expression; `expr` is the column or expression text;
/// `descending` and `nulls_first` are the `indoption` bits (only key columns have them).
/// `opclass` is a key's operator class when it is not the default one, `collation_name` its
/// collation when it differs from the column's (or the key type's), both as a bare name when
/// visible on the search path, else as `"schema"."name"`; `has_options` is set when the
/// operator class has parameters (13+).
pub(crate) const INDEX_COLUMNS: &str = "\
SELECT i.indexrelid::int8 AS idx, \
       k.n AS ord, \
       a.attname::text AS name, \
       pg_catalog.pg_get_indexdef(i.indexrelid, k.n, true) AS expr, \
       COALESCE((i.indoption[k.n - 1]::int4 & 1) = 1, false) AS descending, \
       COALESCE((i.indoption[k.n - 1]::int4 & 2) = 2, false) AS nulls_first, \
       CASE WHEN oc.oid IS NOT NULL AND NOT oc.opcdefault THEN \
            CASE WHEN pg_catalog.pg_opclass_is_visible(oc.oid) THEN oc.opcname::text \
            ELSE '\"' || pg_catalog.replace(ocn.nspname::text, '\"', '\"\"') || '\".\"' || \
                 pg_catalog.replace(oc.opcname::text, '\"', '\"\"') || '\"' END \
       END AS opclass, \
       CASE WHEN kc.oid IS NOT NULL AND kc.oid <> COALESCE(a.attcollation, it.typcollation) THEN \
            CASE WHEN pg_catalog.pg_collation_is_visible(kc.oid) THEN kc.collname::text \
            ELSE '\"' || pg_catalog.replace(kcn.nspname::text, '\"', '\"\"') || '\".\"' || \
                 pg_catalog.replace(kc.collname::text, '\"', '\"\"') || '\"' END \
       END AS collation_name, \
       ia.attoptions IS NOT NULL AS has_options \
FROM pg_catalog.pg_index i \
CROSS JOIN LATERAL pg_catalog.generate_series(1, i.indnatts::int4) AS k(n) \
LEFT JOIN pg_catalog.pg_attribute a \
       ON a.attrelid = i.indrelid AND a.attnum = i.indkey[k.n - 1] AND i.indkey[k.n - 1] <> 0 \
LEFT JOIN pg_catalog.pg_attribute ia ON ia.attrelid = i.indexrelid AND ia.attnum = k.n \
LEFT JOIN pg_catalog.pg_type it ON it.oid = ia.atttypid \
LEFT JOIN pg_catalog.pg_opclass oc ON oc.oid = i.indclass[k.n - 1] \
LEFT JOIN pg_catalog.pg_namespace ocn ON ocn.oid = oc.opcnamespace \
LEFT JOIN pg_catalog.pg_collation kc ON kc.oid = i.indcollation[k.n - 1] \
LEFT JOIN pg_catalog.pg_namespace kcn ON kcn.oid = kc.collnamespace \
WHERE i.indrelid = $1 \
ORDER BY 1, 2";

/// User triggers of relation `$1` (oid). The `WHEN (...)` condition is cut out of `definition`
/// (the pretty-printed `pg_get_triggerdef`) by the caller.
pub(crate) const TRIGGERS: &str = "\
SELECT t.oid::int8 AS oid, \
       t.tgname::text AS name, \
       t.tgtype::int4 AS tgtype, \
       t.tgenabled::text AS enabled, \
       pg_catalog.pg_get_triggerdef(t.oid, true) AS definition, \
       fn.nspname::text AS fn_schema, \
       p.proname::text AS fn_name, \
       pg_catalog.pg_get_function_identity_arguments(p.oid) AS fn_signature, \
       pg_catalog.obj_description(t.oid, 'pg_trigger') AS comment \
FROM pg_catalog.pg_trigger t \
JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid \
JOIN pg_catalog.pg_namespace fn ON fn.oid = p.pronamespace \
WHERE t.tgrelid = $1 AND NOT t.tgisinternal \
ORDER BY t.tgname";

/// `UPDATE OF` columns of trigger `$1` (oid), in declaration order.
pub(crate) const TRIGGER_COLUMNS: &str = "\
SELECT a.attname::text AS name \
FROM pg_catalog.pg_trigger t \
CROSS JOIN LATERAL pg_catalog.unnest(t.tgattr::int2[]) WITH ORDINALITY AS k(attnum, ord) \
JOIN pg_catalog.pg_attribute a ON a.attrelid = t.tgrelid AND a.attnum = k.attnum \
WHERE t.oid = $1 \
ORDER BY k.ord";

/// Routines of schema `$1` matching `$filter`; with `$by_name`, only those named `$2`.
macro_rules! routines {
    ($filter:literal, $by_name:literal) => {
        concat!(
            "SELECT p.oid::int8 AS oid, ",
            "p.proname::text AS name, ",
            "pg_catalog.pg_get_function_identity_arguments(p.oid) AS signature, ",
            "pg_catalog.obj_description(p.oid, 'pg_proc') AS comment, ",
            "EXISTS (SELECT 1 FROM pg_catalog.pg_depend d ",
            "WHERE d.classid = 'pg_catalog.pg_proc'::regclass AND d.objid = p.oid ",
            "AND d.deptype = 'e') AS extension_member ",
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
            "SELECT pg_catalog.pg_get_function_result(p.oid) AS returns, ",
            "l.lanname::text AS language, ",
            "p.prosrc AS src, ",
            $sql_body,
            " AS sql_body, ",
            "pg_catalog.pg_get_functiondef(p.oid) AS definition, ",
            "pg_catalog.obj_description(p.oid, 'pg_proc') AS comment, ",
            "p.provolatile::text AS volatility, ",
            "p.proisstrict AS is_strict, ",
            "p.prosecdef AS security_definer, ",
            "p.proparallel::text AS parallel, ",
            "pg_catalog.pg_get_userbyid(p.proowner)::text AS owner ",
            "FROM pg_catalog.pg_proc p ",
            "JOIN pg_catalog.pg_language l ON l.oid = p.prolang ",
            "WHERE p.oid = $1"
        )
    };
}

/// [`routine`] for PostgreSQL 14+.
pub(crate) const ROUTINE_14: &str = routine!(
    "CASE WHEN p.prosqlbody IS NOT NULL THEN pg_catalog.pg_get_function_sqlbody(p.oid) END"
);
/// [`routine`] before PostgreSQL 14.
pub(crate) const ROUTINE_10: &str = routine!("NULL::text");

/// Arguments of routine `$1` (oid) in declaration order, including `OUT` and `TABLE` columns.
pub(crate) const ROUTINE_ARGS: &str = "\
SELECT a.n AS ord, \
       (p.proargnames)[a.n] AS name, \
       COALESCE((p.proargmodes)[a.n]::text, 'i') AS mode, \
       pg_catalog.format_type(COALESCE((p.proallargtypes)[a.n], p.proargtypes[a.n - 1]), NULL) AS native_type, \
       pg_catalog.pg_get_function_arg_default(p.oid, a.n) AS default_expr \
FROM pg_catalog.pg_proc p \
CROSS JOIN LATERAL pg_catalog.generate_series(1, COALESCE(pg_catalog.array_length(p.proallargtypes, 1), p.pronargs::int4)) AS a(n) \
WHERE p.oid = $1 \
ORDER BY a.n";

/// One sequence by schema (`$1`) and name (`$2`), with the column that owns it.
pub(crate) const SEQUENCE: &str = "\
SELECT pg_catalog.format_type(s.seqtypid, NULL) AS data_type, \
       s.seqstart AS start_value, \
       s.seqincrement AS increment_by, \
       s.seqmin AS min_value, \
       s.seqmax AS max_value, \
       s.seqcache AS cache_size, \
       s.seqcycle AS is_cycle, \
       pg_catalog.obj_description(c.oid, 'pg_class') AS comment, \
       pg_catalog.pg_get_userbyid(c.relowner)::text AS owner, \
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
       pg_catalog.pg_get_viewdef(c.oid, true) AS definition, \
       pg_catalog.obj_description(c.oid, 'pg_class') AS comment, \
       pg_catalog.pg_get_userbyid(c.relowner)::text AS owner, \
       c.relispopulated AS populated, \
       ts.spcname::text AS tablespace, \
       (SELECT o.option_value FROM pg_catalog.pg_options_to_table(c.reloptions) o \
         WHERE o.option_name = 'check_option') AS check_option \
FROM pg_catalog.pg_class c \
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
LEFT JOIN pg_catalog.pg_tablespace ts ON ts.oid = c.reltablespace \
WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('v', 'm')";

/// User-defined types of schema `$1`: enums, standalone composites, domains, ranges and
/// multiranges (array types and table row types are left out).
pub(crate) const TYPES: &str = "\
SELECT t.typname::text AS name, \
       pg_catalog.obj_description(t.oid, 'pg_type') AS comment, \
       t.typtype::text AS typtype, \
       EXISTS (SELECT 1 FROM pg_catalog.pg_depend d \
                WHERE d.classid = 'pg_catalog.pg_type'::regclass AND d.objid = t.oid \
                  AND d.deptype = 'e') AS extension_member \
FROM pg_catalog.pg_type t \
JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
WHERE n.nspname = $1 AND t.typtype IN ('c', 'd', 'e', 'r', 'm') \
  AND (t.typtype <> 'c' OR EXISTS (SELECT 1 FROM pg_catalog.pg_class c \
                                    WHERE c.oid = t.typrelid AND c.relkind = 'c')) \
ORDER BY t.typname";

/// One type by schema (`$1`) and name (`$2`).
pub(crate) const TYPE: &str = "\
SELECT t.typtype::text AS typtype, \
       pg_catalog.obj_description(t.oid, 'pg_type') AS comment, \
       pg_catalog.pg_get_userbyid(t.typowner)::text AS owner, \
       CASE WHEN t.typtype = 'd' THEN pg_catalog.format_type(t.typbasetype, t.typtypmod) END AS base_type, \
       (SELECT pg_catalog.string_agg(pg_catalog.quote_literal(e.enumlabel), ', ' ORDER BY e.enumsortorder) \
          FROM pg_catalog.pg_enum e WHERE e.enumtypid = t.oid) AS enum_labels \
FROM pg_catalog.pg_type t \
JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
WHERE n.nspname = $1 AND t.typname = $2";

/// Extensions installed in the session's database.
pub(crate) const EXTENSIONS: &str = "\
SELECT e.extname::text AS name, \
       pg_catalog.obj_description(e.oid, 'pg_extension') AS comment, \
       e.extversion AS version, \
       n.nspname::text AS schema_name \
FROM pg_catalog.pg_extension e \
JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace \
ORDER BY e.extname";

/// One extension by name (`$1`).
pub(crate) const EXTENSION: &str = "\
SELECT e.extname::text AS name, \
       pg_catalog.obj_description(e.oid, 'pg_extension') AS comment, \
       e.extversion AS version, \
       n.nspname::text AS schema_name, \
       pg_catalog.pg_get_userbyid(e.extowner)::text AS owner \
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

/// The constraint query for `v`.
pub(crate) fn constraints(v: &ServerVersion) -> &'static str {
    if at_least(v, 18) {
        CONSTRAINTS_18
    } else if at_least(v, 11) {
        CONSTRAINTS_11
    } else {
        CONSTRAINTS_10
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
    CONSTRAINTS_18,
    CONSTRAINTS_11,
    CONSTRAINTS_10,
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
    fn catalog_functions_are_qualified() {
        const FUNCTIONS: &[&str] = &[
            "obj_description",
            "shobj_description",
            "col_description",
            "current_database",
            "format_type",
            "pg_get_expr",
            "pg_get_userbyid",
            "pg_get_viewdef",
            "pg_get_functiondef",
            "pg_get_triggerdef",
            "pg_get_indexdef",
            "pg_get_constraintdef",
            "pg_options_to_table",
            "pg_collation_is_visible",
            "pg_opclass_is_visible",
            "array_to_json",
            "replace",
            "unnest",
            "generate_series",
            "array_length",
            "string_agg",
            "quote_literal",
        ];
        for sql in ALL {
            for name in FUNCTIONS {
                let call = format!("{name}(");
                for (at, _) in sql.match_indices(&call) {
                    let before = &sql[..at];
                    // `pg_catalog.name(`, or a longer name ending in `name` (`shobj_description`).
                    let qualified = before.ends_with("pg_catalog.")
                        || before
                            .chars()
                            .last()
                            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
                    assert!(qualified, "unqualified {name}() in {sql}");
                }
            }
        }
    }

    #[test]
    fn constraint_queries_skip_partition_clones_from_11() {
        let v10 = ServerVersion::new(10, 23, 0);
        let v11 = ServerVersion::new(11, 22, 0);
        let v17 = ServerVersion::new(17, 6, 0);
        let v18 = ServerVersion::new(18, 0, 0);
        assert!(!constraints(&v10).contains("conparentid"));
        assert!(constraints(&v11).contains("conparentid"));
        assert!(!constraints(&v17).contains("conenforced"));
        assert!(constraints(&v18).contains("conenforced"));
        assert!(constraints(&v18).contains("conparentid"));
        // The filter sits inside the WHERE clause, before ORDER BY.
        for sql in [CONSTRAINTS_18, CONSTRAINTS_11, CONSTRAINTS_10] {
            assert!(sql.trim_end().ends_with("ORDER BY k.conname"), "{sql}");
        }
        assert!(INDEXES_15.contains("('u', 'x')"));
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
