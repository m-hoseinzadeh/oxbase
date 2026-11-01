//! Live checks of the SQL dump writer: each engine's dump of its own table runs on that engine's
//! fixture and gives back the same rows (docs/m5-contracts.md §3.7, §13 write stream).
//!
//! All are `#[ignore]`d; they need the Docker fixtures (`tests/fixtures.toml`) and run with
//! `cargo test -p oxbase-io -- --include-ignored`. Each test makes a scratch table
//! `m5_t_dump_<token>` (the `[m5]` scratch prefix), fills it with native SQL, exports it with
//! `SELECT *` (the driver's own Arrow types, as an export of the table gets them), deletes the
//! rows, runs the dump statement by statement on the same session and compares a `SELECT` of the
//! rows before and after. The table is dropped afterwards whatever happened.
//!
//! What each engine covers: values with quotes, backslashes, Unicode, NULL and binary;
//! PostgreSQL a `GENERATED ALWAYS` identity, a generated column and a dump run with
//! `standard_conforming_strings` off (a backslash before a quote); MySQL / MariaDB a
//! `TIMESTAMP` column dumped from a session whose time zone is not UTC and whose `sql_mode`
//! has `NO_BACKSLASH_ESCAPES`, and a generated column;
//! SQL Server `datetime` ticks, `smalldatetime`, `datetime2(7)`, an identity and a `rowversion`
//! column.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arrow_array::RecordBatch;
use arrow_array::cast::AsArray;
use oxbase_dialect::DialectRegistry;
use oxbase_driver::{CancellationToken, Collected, ConnectParams, Connection, Driver, collect};
use oxbase_io::{SqlDumpOptions, WriteContext, WriteOptions, create_writer};
use oxbase_types::{
    ColumnDef, DataType, Engine, GeneratedColumn, IdentityDef, ObjectRef, TableDef, TlsMode,
    TlsOptions,
};

type Step<T> = Result<T, String>;

// --- fixtures -----------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn fixtures() -> toml::Table {
    let path = std::env::var("OXBASE_FIXTURES")
        .ok()
        .filter(|p| !p.is_empty())
        .map_or_else(
            || workspace_root().join("tests/fixtures.toml"),
            |p| workspace_root().join(p),
        );
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.parse().expect("fixtures.toml parses")
}

/// The string or integer at `path` of the fixtures, as text.
fn setting(f: &toml::Table, path: &[&str]) -> String {
    let mut value = f
        .get(path[0])
        .unwrap_or_else(|| panic!("fixtures.toml has no {}", path.join(".")));
    for key in &path[1..] {
        value = value
            .get(*key)
            .unwrap_or_else(|| panic!("fixtures.toml has no {}", path.join(".")));
    }
    match value {
        toml::Value::String(v) => v.clone(),
        toml::Value::Integer(v) => v.to_string(),
        other => panic!("{}: unexpected {other:?}", path.join(".")),
    }
}

/// Fixture key, engine and driver of each server engine.
fn engine_of(key: &str) -> Engine {
    match key {
        "postgres" => Engine::Postgres,
        "mysql" => Engine::MySql,
        "mariadb" => Engine::MariaDb,
        "mssql" => Engine::MsSql,
        other => panic!("no live dump test for {other}"),
    }
}

async fn connect(key: &str) -> Box<dyn Connection> {
    let f = fixtures();
    let engine = engine_of(key);
    let port: u16 = setting(&f, &["engines", key, "port"])
        .parse()
        .expect("port");
    let params = ConnectParams::network(engine, setting(&f, &["engines", key, "host"]))
        .with_port(port)
        .with_user(setting(&f, &["engines", key, "user"]))
        .with_password(setting(&f, &["engines", key, "password"]))
        .with_database(setting(&f, &["m5", key, "database"]))
        .with_tls(TlsOptions::new(TlsMode::Disable));
    let connected = match engine {
        Engine::Postgres => oxbase_pg::PgDriver::new().connect(&params).await,
        Engine::MySql | Engine::MariaDb => oxbase_mysql::MySqlDriver::new().connect(&params).await,
        _ => oxbase_mssql::MsSqlDriver::new().connect(&params).await,
    };
    connected.unwrap_or_else(|e| panic!("connect {key}: {e}"))
}

fn dialects() -> Arc<DialectRegistry> {
    let mut reg = DialectRegistry::new();
    oxbase_pg::dialect::register(&mut reg);
    oxbase_mysql::dialect::register(&mut reg);
    oxbase_mssql::dialect::register(&mut reg);
    Arc::new(reg)
}

/// A scratch table name: `m5_t_dump_` plus base36 unix seconds and the process id.
fn scratch_name() -> String {
    fn base36(mut n: u64, width: usize) -> String {
        let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let mut out = vec![b'0'; width];
        for slot in out.iter_mut().rev() {
            *slot = digits[usize::try_from(n % 36).unwrap_or(0)];
            n /= 36;
        }
        String::from_utf8(out).unwrap_or_default()
    }
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!(
        "m5_t_dump_{}{}",
        base36(secs, 6),
        base36(u64::from(std::process::id()), 4)
    )
}

// --- running SQL --------------------------------------------------------------------------

async fn run(c: &dyn Connection, sql: &str) -> Step<Collected> {
    let stream = c
        .execute(sql, &[], CancellationToken::new())
        .await
        .map_err(|e| format!("{sql}: {e}"))?;
    collect(stream).await.map_err(|e| format!("{sql}: {e}"))
}

/// The batches of the first result set of `sql`.
async fn select(c: &dyn Connection, sql: &str) -> Step<Vec<RecordBatch>> {
    let out = run(c, sql).await?;
    let set = out
        .result_sets
        .into_iter()
        .next()
        .ok_or_else(|| format!("{sql}: no result set"))?;
    Ok(set.batches)
}

/// Writes the dump of `batches` for `engine` and returns its text.
fn dump(engine: Engine, table: TableDef, batches: &[RecordBatch]) -> Step<String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("dump.sql");
    let schema = batches
        .first()
        .map(RecordBatch::schema)
        .ok_or("nothing to dump")?;
    let options = WriteOptions::SqlDump(SqlDumpOptions {
        include_create: false,
        rows_per_insert: 2,
        ..SqlDumpOptions::new(engine)
    });
    let context = WriteContext {
        dialects: Some(dialects()),
        table: Some(table),
        source_engine: Some(engine),
    };
    let mut writer =
        create_writer(&path, false, schema, &options, &context).map_err(|e| e.to_string())?;
    for batch in batches {
        writer.write(batch).map_err(|e| e.to_string())?;
    }
    writer.finish().map_err(|e| e.to_string())?;
    std::fs::read_to_string(&path).map_err(|e| e.to_string())
}

/// Runs every statement of `script` on `c`, in order.
async fn run_dump(c: &dyn Connection, engine: Engine, script: &str) -> Step<()> {
    let dialect = oxbase_sql::SqlDialect::for_engine(engine).ok_or("no SQL dialect")?;
    for statement in oxbase_sql::split(dialect, script) {
        if statement.text.trim().is_empty() {
            continue;
        }
        run(c, &statement.text)
            .await
            .map_err(|e| format!("{e}\n--- dump ---\n{script}"))?;
    }
    Ok(())
}

/// Fills `name` with `inserts`, exports it with `export`, deletes the rows, runs the dump and
/// compares `compare` before and after. `before_dump` runs right before the dump (session
/// settings), `before_read` right after it.
struct Case<'a> {
    engine: Engine,
    table: TableDef,
    inserts: &'a [String],
    export: String,
    compare: String,
    before_dump: &'a [&'a str],
    before_read: &'a [&'a str],
}

async fn round_trip(c: &dyn Connection, case: Case<'_>) -> Step<String> {
    for sql in case.inserts {
        run(c, sql).await?;
    }
    let before = select(c, &case.compare).await?;
    let exported = select(c, &case.export).await?;
    let text = dump(case.engine, case.table.clone(), &exported)?;
    run(
        c,
        &format!("DELETE FROM {}", quoted(case.engine, &case.table.obj.name)),
    )
    .await?;
    for sql in case.before_dump {
        run(c, sql).await?;
    }
    run_dump(c, case.engine, &text).await?;
    for sql in case.before_read {
        run(c, sql).await?;
    }
    let after = select(c, &case.compare).await?;
    if before != after {
        return Err(format!(
            "rows differ after the dump\nbefore: {before:?}\nafter: {after:?}\n--- dump ---\n{text}"
        ));
    }
    Ok(text)
}

fn quoted(engine: Engine, name: &str) -> String {
    match engine {
        Engine::MySql | Engine::MariaDb => format!("`{name}`"),
        Engine::MsSql => format!("[{name}]"),
        _ => format!("\"{name}\""),
    }
}

fn column(name: &str, data_type: DataType, native: &str) -> ColumnDef {
    let mut c = ColumnDef::new(name, data_type);
    native.clone_into(&mut c.native_type);
    c
}

/// Text with a quote, a backslash, a line break and Unicode, as a literal of the engine.
const TRICKY_SQL: &str = "'it''s \\ back\nnaïve ☃ 🚀'";

// --- tests --------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs the PostgreSQL fixture"]
async fn postgres_dump_runs_on_postgres() {
    let c = connect("postgres").await;
    let name = scratch_name();
    let create = format!(
        "CREATE TABLE \"{name}\" (id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
         name text, t timestamp(6), tz timestamptz, bin bytea, \
         total integer GENERATED ALWAYS AS (id * 2) STORED)"
    );
    run(c.as_ref(), &create).await.unwrap();
    let mut table = TableDef::new(ObjectRef::table(None, Some("public"), name.clone()));
    let mut id = column("id", DataType::INT32, "integer");
    id.nullable = false;
    id.identity = Some(IdentityDef {
        always: true,
        start: None,
        increment: None,
    });
    let mut total = column("total", DataType::INT32, "integer");
    total.generated = Some(GeneratedColumn {
        expression: "(id * 2)".to_owned(),
        stored: true,
    });
    table.columns = vec![
        id,
        column("name", DataType::Text, "text"),
        column(
            "t",
            DataType::Timestamp { precision: Some(6) },
            "timestamp(6)",
        ),
        column(
            "tz",
            DataType::TimestampTz { precision: Some(6) },
            "timestamptz",
        ),
        column("bin", DataType::VarBinary { length: None }, "bytea"),
        total,
    ];
    let inserts = [
        format!(
            "INSERT INTO \"{name}\" (name, t, tz, bin) VALUES ({TRICKY_SQL}, \
             '2024-01-02 10:00:00.123456', '2024-01-02 10:00:00.5+03:00', '\\x00ff'::bytea)"
        ),
        format!("INSERT INTO \"{name}\" (name, t, tz, bin) VALUES (NULL, NULL, NULL, NULL)"),
        // A backslash before a quote: with standard_conforming_strings off, `'\''...` would end
        // the string early and run the rest as SQL.
        format!("INSERT INTO \"{name}\" (name) VALUES ('\\''); DROP TABLE \"{name}\"; --')"),
        format!("INSERT INTO \"{name}\" (name) VALUES ('ends with \\')"),
    ];
    // The dump runs in a session where standard_conforming_strings is off (a role or database
    // setting): its own `SET standard_conforming_strings = on` makes the literals read back.
    let result = round_trip(
        c.as_ref(),
        Case {
            engine: Engine::Postgres,
            table,
            inserts: &inserts,
            export: format!("SELECT * FROM \"{name}\" ORDER BY id"),
            compare: format!("SELECT * FROM \"{name}\" ORDER BY id"),
            before_dump: &["SET standard_conforming_strings = off"],
            before_read: &[],
        },
    )
    .await;
    let _ = run(c.as_ref(), &format!("DROP TABLE IF EXISTS \"{name}\"")).await;
    let text = result.unwrap();
    assert!(text.contains("OVERRIDING SYSTEM VALUE"), "{text}");
    assert!(
        text.contains("SET standard_conforming_strings = on;"),
        "{text}"
    );
}

async fn mysql_family(key: &str) {
    let engine = engine_of(key);
    let c = connect(key).await;
    let name = scratch_name();
    let create = format!(
        "CREATE TABLE `{name}` (id int PRIMARY KEY, name text, ts timestamp(6) NULL, \
         dt datetime(6), bin varbinary(32), g int GENERATED ALWAYS AS (id * 2) STORED)"
    );
    run(c.as_ref(), &create).await.unwrap();
    let mut table = TableDef::new(ObjectRef::table(None, None, name.clone()));
    let mut id = column("id", DataType::INT32, "int");
    id.nullable = false;
    let mut g = column("g", DataType::INT32, "int");
    g.generated = Some(GeneratedColumn {
        expression: "(`id` * 2)".to_owned(),
        stored: true,
    });
    table.columns = vec![
        id,
        column("name", DataType::Text, "text"),
        column(
            "ts",
            DataType::TimestampTz { precision: Some(6) },
            "timestamp(6)",
        ),
        column(
            "dt",
            DataType::Timestamp { precision: Some(6) },
            "datetime(6)",
        ),
        column(
            "bin",
            DataType::VarBinary { length: Some(32) },
            "varbinary(32)",
        ),
        g,
    ];
    // The driver's session is UTC; the rows are filled and read in UTC.
    let inserts = [
        format!(
            "INSERT INTO `{name}` (id, name, ts, dt, bin) VALUES (1, {}, \
             '2024-01-02 10:00:00.123456', '2024-01-02 10:00:00.5', X'00ff5c27')",
            TRICKY_SQL.replace('\\', "\\\\")
        ),
        format!("INSERT INTO `{name}` (id, name, ts, dt, bin) VALUES (2, NULL, NULL, NULL, NULL)"),
    ];
    // The dump runs in a session at +05:00 with sql_mode NO_BACKSLASH_ESCAPES: its own
    // `SET time_zone` makes the TIMESTAMP values land on the same instants, its `sql_mode`
    // reads the escaped backslashes as one, and both settings are restored afterwards.
    let result = round_trip(
        c.as_ref(),
        Case {
            engine,
            table,
            inserts: &inserts,
            export: format!("SELECT * FROM `{name}` ORDER BY id"),
            compare: format!("SELECT * FROM `{name}` ORDER BY id"),
            before_dump: &[
                "SET time_zone = '+05:00'",
                "SET sql_mode = CONCAT_WS(',', NULLIF(@@session.sql_mode, ''), \
                 'NO_BACKSLASH_ESCAPES')",
            ],
            // The settings the dump left behind, then UTC again for the driver's TIMESTAMP
            // reads.
            before_read: &[
                "SET @oxbase_zone_after = @@session.time_zone",
                "SET @oxbase_mode_after = @@session.sql_mode",
                "SET time_zone = '+00:00'",
            ],
        },
    )
    .await;
    let restored = select(
        c.as_ref(),
        "SELECT IF(@oxbase_zone_after = '+05:00' \
         AND FIND_IN_SET('NO_BACKSLASH_ESCAPES', @oxbase_mode_after) > 0, 1, 0) AS restored",
    )
    .await;
    let _ = run(c.as_ref(), &format!("DROP TABLE IF EXISTS `{name}`")).await;
    // TIMESTAMP values read back the same only if the dump stored the same instants.
    let text = result.unwrap();
    assert!(text.contains("SET time_zone = '+00:00';"), "{text}");
    let restored = restored.unwrap();
    let flag = restored
        .first()
        .map(|b| arrow_cast::cast(b.column(0), &arrow_schema::DataType::Int64).unwrap());
    let flag = flag
        .as_ref()
        .map(|a| a.as_primitive::<arrow_array::types::Int64Type>().value(0));
    assert_eq!(
        flag,
        Some(1),
        "the session time zone and sql_mode are restored"
    );
}

#[tokio::test]
#[ignore = "needs the MySQL fixture"]
async fn mysql_dump_runs_on_mysql() {
    mysql_family("mysql").await;
}

#[tokio::test]
#[ignore = "needs the MariaDB fixture"]
async fn mariadb_dump_runs_on_mariadb() {
    mysql_family("mariadb").await;
}

#[tokio::test]
#[ignore = "needs the SQL Server fixture"]
async fn mssql_dump_runs_on_sql_server() {
    let c = connect("mssql").await;
    let name = scratch_name();
    let create = format!(
        "CREATE TABLE [dbo].[{name}] (id int IDENTITY(1,1) PRIMARY KEY, name nvarchar(100), \
         dt datetime, sdt smalldatetime, dt2 datetime2(7), bin varbinary(32), rv rowversion)"
    );
    run(c.as_ref(), &create).await.unwrap();
    let mut table = TableDef::new(ObjectRef::table(None, Some("dbo"), name.clone()));
    let mut id = column("id", DataType::INT32, "int");
    id.nullable = false;
    id.identity = Some(IdentityDef {
        always: false,
        start: Some(1),
        increment: Some(1),
    });
    let mut rv = column("rv", DataType::Binary { length: Some(8) }, "rowversion");
    rv.nullable = false;
    rv.extras.insert(oxbase_mssql::extras::ROW_VERSION, true);
    table.columns = vec![
        id,
        column(
            "name",
            DataType::VarChar { length: Some(100) },
            "nvarchar(100)",
        ),
        column("dt", DataType::Timestamp { precision: Some(3) }, "datetime"),
        column(
            "sdt",
            DataType::Timestamp { precision: Some(0) },
            "smalldatetime",
        ),
        column(
            "dt2",
            DataType::Timestamp { precision: Some(7) },
            "datetime2(7)",
        ),
        column(
            "bin",
            DataType::VarBinary { length: Some(32) },
            "varbinary(32)",
        ),
        rv,
    ];
    // `datetime` keeps 1/300 s ticks: .003 and .997 are read as .003333 and .996667.
    let inserts = [
        format!(
            "INSERT INTO [dbo].[{name}] (name, dt, sdt, dt2, bin) VALUES (N{TRICKY_SQL}, \
             '2024-01-02T10:00:00.003', '2024-01-02T10:00:00', \
             '2024-01-02T10:00:00.1234567', 0x00ff5c27)"
        ),
        format!(
            "INSERT INTO [dbo].[{name}] (name, dt, sdt, dt2, bin) VALUES (N'x', \
             '2024-01-02T23:59:59.997', '2024-01-02T23:59:00', '0001-01-01T00:00:00', 0x)"
        ),
        format!(
            "INSERT INTO [dbo].[{name}] (name, dt, sdt, dt2, bin) VALUES \
             (NULL, NULL, NULL, NULL, NULL)"
        ),
    ];
    let result = round_trip(
        c.as_ref(),
        Case {
            engine: Engine::MsSql,
            table,
            inserts: &inserts,
            export: format!("SELECT * FROM [dbo].[{name}] ORDER BY id"),
            compare: format!("SELECT id, name, dt, sdt, dt2, bin FROM [dbo].[{name}] ORDER BY id"),
            before_dump: &[],
            before_read: &[],
        },
    )
    .await;
    let _ = run(c.as_ref(), &format!("DROP TABLE IF EXISTS [dbo].[{name}]")).await;
    let text = result.unwrap();
    assert!(text.contains("AS datetime2(7))"), "{text}");
    assert!(text.contains("-- column rv left out"), "{text}");
}
