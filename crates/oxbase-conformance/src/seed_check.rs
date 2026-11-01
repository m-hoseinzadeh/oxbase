//! Live check that the SQL Server fixture's init scripts (`tests/docker/init/mssql/*.sql`,
//! applied by its entrypoint) created everything `tests/fixtures.toml` names in `[m2.mssql]`,
//! `[m3]` / `[m3.mssql]` and `[m4]` / `[m4.mssql]`, with the seed rows the live tests rely on.
//!
//! It reads the server's own catalog through the real driver (no dialect fetcher), so it runs
//! before the SQL Server dialect cells are registered and tells a seed mistake apart from a
//! fetcher mistake. Names are always bound parameters or bracket-quoted
//! ([`BracketQuoting`]), never formatted into SQL unquoted. The non-ASCII values of `m4_types`
//! row 1 (built with `CHAR` / `NCHAR` so the scripts stay ASCII) are compared byte for byte with
//! the text of `docs/m4-contracts.md` §5.3, bound as parameters.

use std::collections::BTreeSet;

use oxbase_dialect::{BracketQuoting, Quoting as _};
use oxbase_driver::{CancellationToken, Collected, Connection};
use oxbase_services::ProvidedSecrets;
use oxbase_types::{Engine, ObjectRef, Value};

use crate::connect::Ctx;
use crate::fixtures::{Fixtures, ResolvedEngine};
use crate::roundtrip::{cell_text, exec, first_text};

/// The database collation the §5.3 values assume (`c_varchar` in code page 1252).
pub const COLLATION: &str = "SQL_Latin1_General_CP1_CI_AS";

/// Every object of the current database, as `schema.name` (indexes and types too), and the
/// database-level triggers as `(database).name`.
const NAMES_SQL: &str = "\
SELECT s.name + N'.' + o.name AS n FROM sys.objects o \
JOIN sys.schemas s ON s.schema_id = o.schema_id WHERE o.is_ms_shipped = 0 \
UNION ALL SELECT s.name + N'.' + i.name FROM sys.indexes i \
JOIN sys.objects o ON o.object_id = i.object_id \
JOIN sys.schemas s ON s.schema_id = o.schema_id WHERE i.name IS NOT NULL AND o.is_ms_shipped = 0 \
UNION ALL SELECT s.name + N'.' + t.name FROM sys.types t \
JOIN sys.schemas s ON s.schema_id = t.schema_id WHERE t.is_user_defined = 1 \
UNION ALL SELECT N'(database).' + t.name FROM sys.triggers t WHERE t.parent_class = 0";

/// The `schema.name` entries [`NAMES_SQL`] must return for the fixture `e` (sorted), or `Err`
/// when the fixtures file lacks a table the check needs.
pub fn expected_names(fixtures: &Fixtures, e: &ResolvedEngine) -> Result<Vec<String>, String> {
    let mut out = BTreeSet::new();
    let (m2, seed) = fixtures
        .m2_seed(&e.name, e.engine)
        .ok_or_else(|| format!("no [m2.{}]", e.name))?;
    let s2 = seed.schema.as_deref().ok_or("[m2.mssql] has no schema")?;
    // Tables, the view and the routines exist when `objects` lists them; the constraints and
    // the index belong to the child table (and the parent's check to the parent).
    let listed = |n: &&String| seed.objects.contains(*n);
    let top = [
        &m2.view,
        &m2.trigger,
        &m2.function,
        &m2.procedure,
        &m2.sequence,
    ];
    out.extend(
        m2.tables
            .iter()
            .chain(top.into_iter().flatten())
            .filter(listed)
            .map(|n| format!("{s2}.{n}")),
    );
    let parent_listed = m2.tables.first().is_some_and(|t| listed(&t));
    let child_listed = m2.tables.get(1).is_some_and(|t| listed(&t));
    let children = [
        (parent_listed, &m2.check_parent),
        (child_listed, &m2.check_child),
        (child_listed, &m2.foreign_key),
        (child_listed, &m2.index),
    ];
    for (on, name) in children {
        if let (true, Some(n)) = (on, name) {
            out.insert(format!("{s2}.{n}"));
        }
    }

    let (m3, seed3) = fixtures
        .m3_seed(&e.name, e.engine)
        .ok_or_else(|| format!("no [m3.{}]", e.name))?;
    let s3 = seed3.schema.as_deref().ok_or("[m3.mssql] has no schema")?;
    let m3_names = [
        &m3.edit_pk_table,
        &m3.edit_unique_table,
        &m3.edit_nokey_table,
        &m3.edit_view,
    ];
    out.extend(m3_names.into_iter().flatten().map(|n| format!("{s3}.{n}")));

    let (m4, _) = fixtures
        .m4_seed(&e.name, e.engine)
        .ok_or_else(|| format!("no [m4.{}]", e.name))?;
    let s4 = m4.schema.as_deref().ok_or("[m4] has no schema")?;
    out.extend(m4.schema_objects().iter().map(|n| format!("{s4}.{n}")));
    for n in [&m4.indexed_view_index, &m4.view_trigger]
        .into_iter()
        .flatten()
    {
        out.insert(format!("{s4}.{n}"));
    }
    if let Some(n) = &m4.edit_audit_table {
        out.insert(format!("{s3}.{n}"));
    }
    if let Some(n) = &m4.ddl_trigger {
        out.insert(format!("(database).{n}"));
    }
    Ok(out.into_iter().collect())
}

/// Runs the check against the fixture `e` (SQL Server only). `Ok` summarizes what was checked;
/// `Err` lists every problem. Must not be called from inside a tokio runtime.
pub fn run(fixtures: &Fixtures, e: &ResolvedEngine) -> Result<String, String> {
    if e.engine != Engine::MsSql {
        return Err(format!("no seed check for {}", e.engine));
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("cannot start the tokio runtime: {err}"))?;
    rt.block_on(async {
        let ctx = Ctx::new(fixtures);
        let p = ctx.profile(e, false).await?;
        let session = ctx
            .mgr
            .open(&p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .map_err(|err| format!("open failed: {err}"))?;
        let out = check(fixtures, e, session.connection().as_ref()).await;
        let _ = ctx.mgr.close(session.id()).await;
        out
    })
}

async fn check(
    fixtures: &Fixtures,
    e: &ResolvedEngine,
    c: &dyn Connection,
) -> Result<String, String> {
    let cancel = CancellationToken::new();
    let want = expected_names(fixtures, e)?;
    let got = exec(c, NAMES_SQL, &[], &cancel).await?;
    let have: BTreeSet<String> = all_text(&got).into_iter().collect();
    let mut problems: Vec<String> = want
        .iter()
        .filter(|n| !have.contains(*n))
        .map(|n| format!("missing {n}"))
        .collect();

    let m2_seed = fixtures.m2_seed(&e.name, e.engine).map(|(_, s)| s);
    if let Some(seed) = m2_seed
        && let (Some(db), Some(table)) = (&seed.other_database, &seed.other_table)
    {
        let full = format!("{db}.dbo.{table}");
        let found = scalar(
            c,
            "SELECT CASE WHEN OBJECT_ID(@P1, N'U') IS NULL THEN 0 ELSE 1 END",
            &[Value::Text(full.clone())],
        )
        .await?;
        if found != "1" {
            problems.push(format!("missing {full}"));
        }
    }

    let q = BracketQuoting;
    let (m3, s3) = fixtures
        .m3_seed(&e.name, e.engine)
        .ok_or_else(|| format!("no [m3.{}]", e.name))?;
    let (m4, s4) = fixtures
        .m4_seed(&e.name, e.engine)
        .ok_or_else(|| format!("no [m4.{}]", e.name))?;
    let db = s4.database.clone().unwrap_or_default();
    let table = |schema: &Option<String>, name: &Option<String>| -> Option<(String, String)> {
        let r = ObjectRef::table(Some(db.as_str()), schema.as_deref(), name.clone()?);
        Some((q.qualify(&r), format!("{}.{}", schema.as_deref()?, r.name)))
    };
    let mut checks: Vec<(String, String, Vec<Value>, String)> = vec![(
        "database collation".to_owned(),
        "SELECT CONVERT(nvarchar(128), DATABASEPROPERTYEX(DB_NAME(), 'Collation'))".to_owned(),
        Vec::new(),
        COLLATION.to_owned(),
    )];
    checks.push((
        "seed marker".to_owned(),
        "SELECT COUNT(*) FROM sys.extended_properties WHERE class = 0 \
         AND name = N'oxbase_fixture_seeded'"
            .to_owned(),
        Vec::new(),
        "1".to_owned(),
    ));
    if let Some((sql_name, plain)) = table(&s3.schema, &m3.edit_pk_table) {
        checks.push((
            format!("{plain} seed rows 1-3"),
            format!(
                "SELECT COUNT(*) FROM {sql_name} WHERE (id = 1 AND code = 'a') \
                 OR (id = 2 AND code = 'b') \
                 OR (id = 3 AND code = 'c' AND name IS NULL AND amount IS NULL AND doc IS NULL)"
            ),
            Vec::new(),
            "3".to_owned(),
        ));
        // Seed 1, step 1, and at least past the seed rows. Not "= 3": the grid and DDL live
        // tests insert and delete their own rows, which moves the identity on a reused volume.
        checks.push((
            format!("{plain} identity seed 1, step 1, at least 3"),
            "SELECT CASE WHEN IDENT_SEED(@P1) = 1 AND IDENT_INCR(@P1) = 1 \
             AND IDENT_CURRENT(@P1) >= 3 THEN 1 ELSE 0 END"
                .to_owned(),
            vec![Value::Text(plain.clone())],
            "1".to_owned(),
        ));
        checks.push((
            format!("{plain} has one DML trigger"),
            "SELECT COUNT(*) FROM sys.triggers WHERE parent_id = OBJECT_ID(@P1)".to_owned(),
            vec![Value::Text(plain.clone())],
            "1".to_owned(),
        ));
    }
    if let Some((sql_name, plain)) = table(&m4.schema, &m4.types_table) {
        checks.push((
            format!("{plain} rows"),
            format!("SELECT COUNT(*) FROM {sql_name}"),
            Vec::new(),
            "2".to_owned(),
        ));
        // Byte-exact: varchar in the database code page, nchar padded, nvarchar with a
        // surrogate pair (U+1F600).
        checks.push((
            format!("{plain} row 1 non-ASCII text"),
            format!(
                "SELECT COUNT(*) FROM {sql_name} WHERE id = 1 \
                 AND CAST(c_varchar AS varbinary(100)) \
                     = CAST(CAST(@P1 AS varchar(50)) AS varbinary(100)) \
                 AND CAST(c_nchar AS varbinary(100)) \
                     = CAST(CAST(@P2 AS nchar(10)) AS varbinary(100)) \
                 AND CAST(c_nvarchar AS varbinary(200)) = CAST(@P3 AS varbinary(200)) \
                 AND DATALENGTH(c_nvarchar) = 22 AND LEN(c_varchar_max) = 9000 \
                 AND LEN(c_nvarchar_max) = 10000"
            ),
            vec![
                Value::Text("caf\u{e9}".to_owned()),
                Value::Text("\u{2603}".to_owned()),
                Value::Text("Gr\u{fc}\u{df}e \u{6f22}\u{5b57} \u{1f600}".to_owned()),
            ],
            "1".to_owned(),
        ));
    }
    if let Some((_, plain)) = table(&m4.schema, &m4.temporal_table) {
        checks.push((
            format!("{plain} is system-versioned"),
            "SELECT CONVERT(int, temporal_type) FROM sys.tables WHERE object_id = OBJECT_ID(@P1)"
                .to_owned(),
            vec![Value::Text(plain.clone())],
            "2".to_owned(),
        ));
    }
    if let (Some((_, plain)), Some(ix)) =
        (table(&m4.schema, &m4.indexed_view), &m4.indexed_view_index)
    {
        checks.push((
            format!("{plain} has the unique clustered index {ix}"),
            "SELECT COUNT(*) FROM sys.indexes WHERE object_id = OBJECT_ID(@P1) AND name = @P2 \
             AND type = 1 AND is_unique = 1"
                .to_owned(),
            vec![Value::Text(plain.clone()), Value::Text(ix.clone())],
            "1".to_owned(),
        ));
    }

    let ran = checks.len();
    for (what, sql, params, expect) in checks {
        match scalar(c, &sql, &params).await {
            Ok(v) if v == expect => {}
            Ok(v) => problems.push(format!("{what}: got {v}, expected {expect}")),
            Err(err) => problems.push(format!("{what}: {err}")),
        }
    }
    if problems.is_empty() {
        Ok(format!(
            "{} named objects present, {ran} value checks passed",
            want.len()
        ))
    } else {
        Err(problems.join("\n  "))
    }
}

/// The first value of `sql` as text (`NULL` as `NULL`).
async fn scalar(c: &dyn Connection, sql: &str, params: &[Value]) -> Result<String, String> {
    let out = exec(c, sql, params, &CancellationToken::new())
        .await
        .map_err(|err| format!("{sql}: {err}"))?;
    Ok(first_text(&out).unwrap_or_else(|| "NULL".to_owned()))
}

/// The first column of every row of every result set, as text.
fn all_text(out: &Collected) -> Vec<String> {
    out.result_sets
        .iter()
        .flat_map(|r| r.batches.iter())
        .flat_map(|b| (0..b.num_rows()).filter_map(move |i| cell_text(b.column(0).as_ref(), i)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::workspace_root;

    #[test]
    fn expected_names_cover_m2_m3_and_m4() {
        let fx =
            Fixtures::from_path(&workspace_root().join("tests").join("fixtures.toml")).unwrap();
        let e = fx
            .resolve()
            .unwrap()
            .into_iter()
            .find(|e| e.engine == Engine::MsSql)
            .unwrap();
        let names = expected_names(&fx, &e).unwrap();
        for n in [
            "m2.m2_parent",
            "m2.m2_child",
            "m2.m2_view",
            "m2.m2_trg",
            "m2.m2_fn",
            "m2.m2_proc",
            "m2.m2_seq",
            "m2.m2_parent_code_chk",
            "m2.m2_child_qty_chk",
            "m2.m2_child_parent_fk",
            "m2.m2_child_parent_idx",
            "m3.m3_edit_pk",
            "m3.m3_edit_uq",
            "m3.m3_edit_nokey",
            "m3.m3_edit_view",
            "m3.m3_edit_audit",
            "m4.m4_types",
            "m4.m4_features",
            "m4.m4_temporal",
            "m4.m4_temporal_history",
            "m4.m4_iview",
            "m4.m4_iview_ix",
            "m4.m4_view_trg",
            "m4.m4_itvf",
            "m4.m4_mtvf",
            "m4.m4_code_t",
            "m4.m4_rows_t",
            "m4.m4_proc_tvp",
            "m4.m4_messages",
            "(database).m4_ddl_trg",
        ] {
            assert!(names.iter().any(|x| x == n), "{n} not in {names:?}");
        }
        assert_eq!(names.len(), 30, "{names:?}");
    }

    #[test]
    fn only_sql_server_has_a_seed_check() {
        let fx =
            Fixtures::from_path(&workspace_root().join("tests").join("fixtures.toml")).unwrap();
        let pg = fx
            .resolve()
            .unwrap()
            .into_iter()
            .find(|e| e.engine == Engine::Postgres)
            .unwrap();
        assert!(run(&fx, &pg).unwrap_err().contains("no seed check"));
    }
}
