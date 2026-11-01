//! SQL dump writer: golden `INSERT`s per target engine (quotes, backslashes, Unicode, NULL,
//! binary), `CREATE` / `DROP` from the target's `DdlGenerator`, table definitions within and
//! across engines, identity columns, statement sizes, and the SQLite dump executed for real
//! (docs/m5-contracts.md §3.7, §12 write).

mod write_support;

use std::sync::Arc;

use arrow_array::{
    ArrayRef, BinaryArray, BooleanArray, Decimal128Array, FixedSizeBinaryArray, Int64Array,
    NullArray, RecordBatch, StringArray, Time64NanosecondArray, TimestampMicrosecondArray,
    TimestampNanosecondArray,
};
use arrow_schema::{DataType as ArrowType, Field, Schema, TimeUnit};
use oxbase_io::{SinkReport, SqlDumpOptions, WriteContext, WriteOptions};
use oxbase_types::{
    CheckDef, ColumnDef, DataType, Engine, ErrorCategory, Extras, GeneratedColumn, IdentityDef,
    ObjectRef, PrimaryKeyDef, TableDef,
};
use pretty_assertions::assert_eq;
use write_support::{
    BEYOND_DOUBLE, TRICKY, UNICODE, UUID_1, UUID_2, date, dialects, micros, numbers, parts, sample,
    utc_micros, write_all,
};

/// Three rows: escapes and Unicode, edge values, NULLs.
fn small() -> RecordBatch {
    let columns: Vec<ArrayRef> = vec![
        Arc::new(BooleanArray::from(vec![Some(true), Some(false), None])),
        Arc::new(Int64Array::from(vec![Some(1), Some(BEYOND_DOUBLE), None])),
        Arc::new(
            Decimal128Array::from(vec![Some(1250), Some(-5), None])
                .with_precision_and_scale(10, 2)
                .unwrap(),
        ),
        Arc::new(StringArray::from(vec![
            Some("it's \\ back"),
            Some("line\nnext ☃ 🚀"),
            None,
        ])),
        Arc::new(BinaryArray::from(vec![
            Some(&b"\x00\xff"[..]),
            Some(&b""[..]),
            None,
        ])),
        Arc::new(
            TimestampMicrosecondArray::from(vec![Some(1_700_000_000_123_456), Some(0), None])
                .with_timezone("+00:00"),
        ),
        Arc::new(
            FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                vec![Some(UUID_1), Some(UUID_2), None].into_iter(),
                16,
            )
            .unwrap(),
        ),
    ];
    let names = ["flag", "n", "dec", "txt", "bin", "at_tz", "id"];
    let fields: Vec<Field> = names
        .iter()
        .zip(&columns)
        .map(|(n, c)| Field::new(*n, c.data_type().clone(), true))
        .collect();
    assert_eq!(fields[5].data_type(), &utc_micros());
    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap()
}

fn context() -> WriteContext {
    WriteContext {
        dialects: Some(dialects()),
        ..WriteContext::default()
    }
}

fn dump(
    engine: Engine,
    options: SqlDumpOptions,
    context: &WriteContext,
    batches: &[RecordBatch],
) -> String {
    dump_report(engine, options, context, batches).0
}

/// [`dump`] with the writer's report.
fn dump_report(
    engine: Engine,
    options: SqlDumpOptions,
    context: &WriteContext,
    batches: &[RecordBatch],
) -> (String, SinkReport) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dump.sql");
    let options = WriteOptions::SqlDump(SqlDumpOptions {
        target: engine,
        ..options
    });
    let report = write_all(&path, &options, context, batches).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(report.bytes, u64::try_from(text.len()).unwrap());
    assert_eq!(parts(dir.path()), Vec::<String>::new());
    (text, report)
}

fn two_per_insert(engine: Engine) -> SqlDumpOptions {
    SqlDumpOptions {
        include_drop: true,
        rows_per_insert: 2,
        ..SqlDumpOptions::new(engine)
    }
}

/// The `INSERT` section expected for [`small`], two rows per statement.
fn golden_inserts(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => {
            "INSERT INTO \"export\" (\"flag\", \"n\", \"dec\", \"txt\", \"bin\", \"at_tz\", \"id\") VALUES
(TRUE, 1, 12.50, 'it''s \\ back', '\\x00ff'::bytea, '2023-11-14T22:13:20.123456+00:00', '12345678-9abc-def0-1234-56789abcdef0'),
(FALSE, 9007199254740993, -0.05, 'line
next ☃ 🚀', '\\x'::bytea, '1970-01-01T00:00:00+00:00', 'ffffffff-ffff-ffff-ffff-ffffffffffff');
INSERT INTO \"export\" (\"flag\", \"n\", \"dec\", \"txt\", \"bin\", \"at_tz\", \"id\") VALUES
(NULL, NULL, NULL, NULL, NULL, NULL, NULL);
"
        }
        // Timestamps with a zone: UTC wall-clock time without an offset (MariaDB and MySQL
        // before 8.0.19 read no offset), under `time_zone = '+00:00'` restored at the end.
        Engine::MySql | Engine::MariaDb => {
            "INSERT INTO `export` (`flag`, `n`, `dec`, `txt`, `bin`, `at_tz`, `id`) VALUES
(TRUE, 1, 12.50, 'it''s \\\\ back', X'00ff', '2023-11-14 22:13:20.123456', '12345678-9abc-def0-1234-56789abcdef0'),
(FALSE, 9007199254740993, -0.05, 'line
next ☃ 🚀', X'', '1970-01-01 00:00:00', 'ffffffff-ffff-ffff-ffff-ffffffffffff');
INSERT INTO `export` (`flag`, `n`, `dec`, `txt`, `bin`, `at_tz`, `id`) VALUES
(NULL, NULL, NULL, NULL, NULL, NULL, NULL);
SET time_zone = @oxbase_time_zone;
SET character_set_client = @oxbase_character_set_client;
SET character_set_results = @oxbase_character_set_results;
SET collation_connection = @oxbase_collation_connection;
SET sql_mode = @oxbase_sql_mode;
"
        }
        // Decimals quoted: an unquoted number is a double to SQLite before the column's
        // affinity applies (a `DECIMAL_TEXT` column would lose digits past about 15).
        Engine::Sqlite => {
            "INSERT INTO \"export\" (\"flag\", \"n\", \"dec\", \"txt\", \"bin\", \"at_tz\", \"id\") VALUES
(TRUE, 1, '12.50', 'it''s \\ back', X'00ff', '2023-11-14T22:13:20.123456+00:00', '12345678-9abc-def0-1234-56789abcdef0'),
(FALSE, 9007199254740993, '-0.05', 'line
next ☃ 🚀', X'', '1970-01-01T00:00:00+00:00', 'ffffffff-ffff-ffff-ffff-ffffffffffff');
INSERT INTO \"export\" (\"flag\", \"n\", \"dec\", \"txt\", \"bin\", \"at_tz\", \"id\") VALUES
(NULL, NULL, NULL, NULL, NULL, NULL, NULL);
"
        }
        Engine::MsSql => {
            "INSERT INTO [export] ([flag], [n], [dec], [txt], [bin], [at_tz], [id]) VALUES
(1, 1, 12.50, N'it''s \\ back', 0x00ff, N'2023-11-14T22:13:20.123456+00:00', N'12345678-9abc-def0-1234-56789abcdef0'),
(0, 9007199254740993, -0.05, N'line
next ☃ 🚀', 0x, N'1970-01-01T00:00:00+00:00', N'ffffffff-ffff-ffff-ffff-ffffffffffff');
INSERT INTO [export] ([flag], [n], [dec], [txt], [bin], [at_tz], [id]) VALUES
(NULL, NULL, NULL, NULL, NULL, NULL, NULL);
"
        }
        Engine::MongoDb | Engine::Redis => unreachable!(),
    }
}

/// What a MySQL / MariaDB dump starts with: `sql_mode` without `NO_BACKSLASH_ESCAPES` and
/// `SET NAMES utf8mb4`, both saved first.
const MYSQL_PROLOGUE: &str = "SET @oxbase_sql_mode = @@session.sql_mode;
SET sql_mode = TRIM(BOTH ',' FROM REPLACE(CONCAT(',', @@session.sql_mode, ','), ',NO_BACKSLASH_ESCAPES,', ','));
SET @oxbase_character_set_client = @@session.character_set_client;
SET @oxbase_character_set_results = @@session.character_set_results;
SET @oxbase_collation_connection = @@session.collation_connection;
SET NAMES utf8mb4;
";

/// What a MySQL / MariaDB dump ends with (after the time zone, when it set one).
const MYSQL_EPILOGUE: &str = "SET character_set_client = @oxbase_character_set_client;
SET character_set_results = @oxbase_character_set_results;
SET collation_connection = @oxbase_collation_connection;
SET sql_mode = @oxbase_sql_mode;
";

#[test]
fn golden_inserts_per_engine() {
    let context = context();
    for (engine, version, create) in [
        (
            Engine::Postgres,
            "PostgreSQL 18.0.0",
            "CREATE TABLE \"export\" (",
        ),
        (Engine::MySql, "MySQL 8.4.0", "CREATE TABLE `export` ("),
        (Engine::MariaDb, "MariaDB 11.8.0", "CREATE TABLE `export` ("),
        (
            Engine::Sqlite,
            "SQLite 3.45.0",
            "CREATE TABLE \"main\".\"export\" (",
        ),
        (
            Engine::MsSql,
            "SQL Server 17.0.0",
            "CREATE TABLE [export] (",
        ),
    ] {
        let text = dump(engine, two_per_insert(engine), &context, &[small()]);
        assert!(
            text.starts_with(&format!("-- Oxbase SQL dump\n-- Target: {version}\n")),
            "{engine}:\n{text}"
        );
        assert!(text.contains(create), "{engine}:\n{text}");
        let drop = text.find("DROP TABLE").expect("a DROP statement");
        assert!(drop < text.find("CREATE TABLE").unwrap(), "{engine}");
        let inserts = &text[text.find("INSERT INTO").unwrap()..];
        assert_eq!(inserts, golden_inserts(engine), "{engine}");
        let session_zone = text.find("SET time_zone = '+00:00';\n");
        if matches!(engine, Engine::MySql | Engine::MariaDb) {
            assert!(
                session_zone.is_some_and(|at| at < drop),
                "{engine}: the session time zone is set first:\n{text}"
            );
            assert!(
                text.find(MYSQL_PROLOGUE).is_some_and(|at| at < drop),
                "{engine}: sql_mode and character set first:\n{text}"
            );
        } else {
            assert!(session_zone.is_none(), "{engine}:\n{text}");
        }
        let conforming = text.find("SET standard_conforming_strings = on;\n");
        if engine == Engine::Postgres {
            assert!(conforming.is_some_and(|at| at < drop), "{engine}:\n{text}");
        } else {
            assert!(conforming.is_none(), "{engine}:\n{text}");
        }
        // Every statement ends with `;` and the file uses `\n` only.
        assert!(!text.contains('\r'), "{engine}");
        assert!(!text.contains("\nGO"), "{engine}");
    }
}

#[test]
fn types_a_target_cannot_declare_fall_back_with_a_note() {
    let text = dump(
        Engine::MySql,
        SqlDumpOptions::new(Engine::MySql),
        &context(),
        &[small()],
    );
    // MySQL has no uuid type: char(36) instead, said in the header.
    assert!(text.contains("-- column id: "), "{text}");
    assert!(text.contains("char(36)"), "{text}");
    // MariaDB 11.8 has one.
    let maria = dump(
        Engine::MariaDb,
        SqlDumpOptions::new(Engine::MariaDb),
        &context(),
        &[small()],
    );
    assert!(!maria.contains("-- column id: "), "{maria}");
}

#[test]
fn every_type_dumps_for_every_engine() {
    let context = context();
    for engine in [
        Engine::Postgres,
        Engine::MySql,
        Engine::MariaDb,
        Engine::Sqlite,
        Engine::MsSql,
    ] {
        let text = dump(
            engine,
            SqlDumpOptions::new(engine),
            &context,
            &[sample(), sample()],
        );
        assert_eq!(text.matches("INSERT INTO").count(), 1, "{engine}");
        // MySQL / MariaDB restore the session time zone after the inserts (`at_tz`), then the
        // character set and sql_mode.
        let end = match engine {
            Engine::MySql | Engine::MariaDb => {
                format!(");\nSET time_zone = @oxbase_time_zone;\n{MYSQL_EPILOGUE}")
            }
            _ => ");\n".to_owned(),
        };
        assert!(text.ends_with(&end), "{engine}:\n{text}");
    }
}

/// `txt`, `bin`, `n`, `flag`, `tags` of one row read back from SQLite.
type Row = (
    Option<String>,
    Option<Vec<u8>>,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

#[test]
fn sqlite_dump_runs_and_reads_back() {
    let options = SqlDumpOptions {
        rows_per_insert: 3,
        ..SqlDumpOptions::new(Engine::Sqlite)
    };
    let text = dump(Engine::Sqlite, options, &context(), &[sample()]);
    assert_eq!(text.matches("INSERT INTO").count(), 2);
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(&text).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT \"txt\", \"bin\", \"n\", \"flag\", \"tags\" FROM \"export\" ORDER BY rowid",
        )
        .unwrap();
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            (
                Some("plain".to_owned()),
                Some(vec![0, 255]),
                Some(1),
                Some(1),
                Some("[1,2]".to_owned())
            ),
            (
                Some(TRICKY.to_owned()),
                Some(Vec::new()),
                Some(BEYOND_DOUBLE),
                Some(0),
                Some("[]".to_owned())
            ),
            (None, None, None, None, None),
            (
                Some(UNICODE.to_owned()),
                Some(b"abc".to_vec()),
                Some(i64::MIN),
                Some(1),
                Some("[3]".to_owned())
            ),
        ]
    );
}

/// `public.orders (id bigint identity always, name text, total bigint generated)`.
fn orders() -> (TableDef, RecordBatch) {
    let mut table = TableDef::new(ObjectRef::table(None, Some("public"), "orders"));
    let mut id = ColumnDef::new("id", DataType::INT64);
    "bigint".clone_into(&mut id.native_type);
    id.nullable = false;
    id.auto_increment = true;
    id.identity = Some(IdentityDef {
        always: true,
        start: None,
        increment: None,
    });
    let mut name = ColumnDef::new("name", DataType::Text);
    "text".clone_into(&mut name.native_type);
    name.comment = Some("who".to_owned());
    let mut total = ColumnDef::new("total", DataType::INT64);
    "bigint".clone_into(&mut total.native_type);
    total.generated = Some(GeneratedColumn {
        expression: "id * 2".to_owned(),
        stored: true,
    });
    table.columns = vec![id, name, total];
    table.primary_key = Some(PrimaryKeyDef {
        name: Some("orders_pkey".to_owned()),
        columns: vec!["id".to_owned()],
    });
    table.checks = vec![CheckDef {
        name: Some("orders_name_check".to_owned()),
        expression: "name <> ''".to_owned(),
        extras: Extras::default(),
    }];
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", ArrowType::Int64, false),
        Field::new("name", ArrowType::Utf8, true),
        Field::new("total", ArrowType::Int64, true),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![1, 2])),
            Arc::new(StringArray::from(vec!["a", "b"])),
            Arc::new(Int64Array::from(vec![2, 4])),
        ],
    )
    .unwrap();
    (table, batch)
}

#[test]
fn same_engine_table_keeps_its_definition() {
    let (table, batch) = orders();
    let context = WriteContext {
        dialects: Some(dialects()),
        table: Some(table),
        source_engine: Some(Engine::Postgres),
    };
    let text = dump(
        Engine::Postgres,
        SqlDumpOptions::new(Engine::Postgres),
        &context,
        &[batch],
    );
    // Unqualified, so the dump loads into the current schema.
    assert!(text.contains("CREATE TABLE \"orders\" ("), "{text}");
    assert!(text.contains("GENERATED ALWAYS AS IDENTITY"), "{text}");
    assert!(text.contains("id * 2"), "{text}");
    assert!(text.contains("orders_name_check"), "{text}");
    // The generated column is computed by the server; identity ALWAYS needs OVERRIDING.
    assert!(
        text.ends_with(
            "INSERT INTO \"orders\" (\"id\", \"name\") OVERRIDING SYSTEM VALUE VALUES\n(1, 'a'),\n(2, 'b');\n"
        ),
        "{text}"
    );
}

#[test]
fn a_renamed_table_renames_its_constraints() {
    // A dump written under another name loads next to its source: PostgreSQL constraint
    // names are unique per schema.
    let (table, batch) = orders();
    let context = WriteContext {
        dialects: Some(dialects()),
        table: Some(table),
        source_engine: Some(Engine::Postgres),
    };
    let options = SqlDumpOptions {
        table: Some(ObjectRef::table(None, Some("public"), "orders_copy")),
        ..SqlDumpOptions::new(Engine::Postgres)
    };
    let text = dump(Engine::Postgres, options, &context, &[batch]);
    assert!(
        text.contains("CREATE TABLE \"public\".\"orders_copy\" ("),
        "{text}"
    );
    assert!(text.contains("\"orders_copy_pkey\""), "{text}");
    assert!(text.contains("\"orders_copy_name_check\""), "{text}");
    assert!(!text.contains("\"orders_pkey\""), "{text}");
    assert!(!text.contains("\"orders_name_check\""), "{text}");
}

#[test]
fn across_engines_the_source_sql_is_dropped() {
    let (table, batch) = orders();
    for engine in [Engine::MySql, Engine::Sqlite, Engine::MsSql] {
        let context = WriteContext {
            dialects: Some(dialects()),
            table: Some(table.clone()),
            source_engine: Some(Engine::Postgres),
        };
        let text = dump(
            engine,
            SqlDumpOptions::new(engine),
            &context,
            std::slice::from_ref(&batch),
        );
        assert!(text.contains("from another engine"), "{engine}:\n{text}");
        assert!(
            text.contains("1 check constraint(s) left out"),
            "{engine}:\n{text}"
        );
        assert!(!text.contains("id * 2"), "{engine}:\n{text}");
        assert!(!text.contains("OVERRIDING"), "{engine}:\n{text}");
        assert!(text.contains("PRIMARY KEY"), "{engine}:\n{text}");
        // The formerly generated column is a plain column now, so its values are written.
        let q = |s: &str| match engine {
            Engine::MySql => format!("`{s}`"),
            Engine::MsSql => format!("[{s}]"),
            _ => format!("\"{s}\""),
        };
        let columns = format!("({}, {}, {})", q("id"), q("name"), q("total"));
        assert!(text.contains(&columns), "{engine}:\n{text}");
    }
}

#[test]
fn sql_server_identity_inserts_and_statement_limit() {
    let (mut table, batch) = orders();
    table.columns[2].generated = None;
    let context = WriteContext {
        dialects: Some(dialects()),
        table: Some(table),
        source_engine: Some(Engine::Postgres),
    };
    let text = dump(
        Engine::MsSql,
        SqlDumpOptions::new(Engine::MsSql),
        &context,
        &[batch],
    );
    let on = text.find("SET IDENTITY_INSERT [orders] ON;\n").expect("ON");
    let insert = text.find("INSERT INTO [orders]").unwrap();
    assert!(on < insert, "{text}");
    assert!(
        text.ends_with(");\nSET IDENTITY_INSERT [orders] OFF;\n"),
        "{text}"
    );

    // SQL Server takes at most 1000 rows per VALUES list, whatever the option says.
    let options = SqlDumpOptions {
        rows_per_insert: 5000,
        ..SqlDumpOptions::new(Engine::MsSql)
    };
    let text = dump(
        Engine::MsSql,
        options,
        &self::context(),
        &[numbers(1500), numbers(501)],
    );
    assert_eq!(text.matches("INSERT INTO").count(), 3);
    let options = SqlDumpOptions {
        rows_per_insert: 5000,
        ..SqlDumpOptions::new(Engine::Postgres)
    };
    let text = dump(
        Engine::Postgres,
        options,
        &self::context(),
        &[numbers(1500), numbers(501)],
    );
    assert_eq!(text.matches("INSERT INTO").count(), 1);
}

#[test]
fn options_name_table_and_skip_create() {
    let options = SqlDumpOptions {
        table: Some(ObjectRef::table(None, Some("sales"), "q1")),
        include_create: false,
        ..SqlDumpOptions::new(Engine::Postgres)
    };
    let text = dump(Engine::Postgres, options, &context(), &[numbers(2)]);
    assert!(!text.contains("CREATE"), "{text}");
    assert!(
        text.ends_with("INSERT INTO \"sales\".\"q1\" (\"n\") VALUES\n(0),\n(1);\n"),
        "{text}"
    );

    let options = SqlDumpOptions {
        target_version: Some("9.6".to_owned()),
        ..SqlDumpOptions::new(Engine::Postgres)
    };
    let text = dump(Engine::Postgres, options, &context(), &[]);
    assert!(
        text.starts_with("-- Oxbase SQL dump\n-- Target: PostgreSQL 9.6.0\n"),
        "{text}"
    );
    assert!(!text.contains("INSERT"), "{text}");
}

#[test]
fn a_dump_needs_the_dialect_cells() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("none.sql");
    let options = WriteOptions::SqlDump(SqlDumpOptions::new(Engine::Postgres));
    let err = write_all(&path, &options, &WriteContext::default(), &[small()]).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Unsupported);
    assert_eq!(write_support::files(dir.path()), Vec::<String>::new());
}

/// One row of date-time values: a SQL Server `datetime` tick as its driver reads it
/// (`10:00:00.003333`), a nanosecond timestamp, a timestamp with a zone and a nanosecond time
/// of day.
fn date_times() -> RecordBatch {
    let nanos = date(2024, 1, 2)
        .and_hms_nano_opt(10, 0, 0, 123_456_789)
        .unwrap()
        .and_utc()
        .timestamp_nanos_opt()
        .unwrap();
    let columns: Vec<ArrayRef> = vec![
        Arc::new(TimestampMicrosecondArray::from(vec![micros(
            2024, 1, 2, 10, 0, 0, 3_333,
        )])),
        Arc::new(TimestampNanosecondArray::from(vec![nanos])),
        Arc::new(
            TimestampMicrosecondArray::from(vec![micros(2024, 1, 2, 10, 0, 0, 500_000)])
                .with_timezone("+00:00"),
        ),
        Arc::new(Time64NanosecondArray::from(vec![
            36_000_000_000_000 + 123_456_789,
        ])),
    ];
    let schema = Schema::new(vec![
        Field::new(
            "at",
            ArrowType::Timestamp(TimeUnit::Microsecond, None),
            true,
        ),
        Field::new(
            "at_ns",
            ArrowType::Timestamp(TimeUnit::Nanosecond, None),
            true,
        ),
        Field::new("at_tz", utc_micros(), true),
        Field::new("clock", ArrowType::Time64(TimeUnit::Nanosecond), true),
    ]);
    RecordBatch::try_new(Arc::new(schema), columns).unwrap()
}

#[test]
fn date_time_literals_suit_the_target() {
    let options = |engine| SqlDumpOptions {
        include_create: false,
        ..SqlDumpOptions::new(engine)
    };
    // SQL Server: naive timestamps through datetime2(7), which converts to `datetime` (and
    // every other date-time type) with rounding; at most 7 fractional digits.
    let (text, report) = dump_report(
        Engine::MsSql,
        options(Engine::MsSql),
        &context(),
        &[date_times()],
    );
    assert!(
        text.ends_with(
            "INSERT INTO [export] ([at], [at_ns], [at_tz], [clock]) VALUES\n\
             (CAST(N'2024-01-02T10:00:00.003333' AS datetime2(7)), \
             CAST(N'2024-01-02T10:00:00.1234567' AS datetime2(7)), \
             N'2024-01-02T10:00:00.5+00:00', N'10:00:00.1234567');\n"
        ),
        "{text}"
    );
    let warnings = report.warnings.join("\n");
    assert!(
        warnings.contains("2 date-time values had more than 7 fractional"),
        "{warnings}"
    );

    // MySQL / MariaDB: no offsets, at most 6 digits, UTC session time zone.
    for engine in [Engine::MySql, Engine::MariaDb] {
        let (text, report) = dump_report(engine, options(engine), &context(), &[date_times()]);
        let set = text
            .find("SET time_zone = '+00:00';\n")
            .expect("time zone set");
        assert!(set < text.find("INSERT").unwrap(), "{text}");
        assert!(
            text.ends_with(&format!(
                "INSERT INTO `export` (`at`, `at_ns`, `at_tz`, `clock`) VALUES\n\
                 ('2024-01-02 10:00:00.003333', '2024-01-02 10:00:00.123456', \
                 '2024-01-02 10:00:00.5', '10:00:00.123456');\n\
                 SET time_zone = @oxbase_time_zone;\n{MYSQL_EPILOGUE}"
            )),
            "{engine}:\n{text}"
        );
        let warnings = report.warnings.join("\n");
        assert!(
            warnings.contains("2 date-time values had more than 6 fractional"),
            "{engine}: {warnings}"
        );
    }

    // PostgreSQL keeps the dialect's literals (it rounds extra digits itself).
    let text = dump(
        Engine::Postgres,
        options(Engine::Postgres),
        &context(),
        &[date_times()],
    );
    assert!(!text.contains("CAST("), "{text}");
    assert!(!text.contains("time_zone"), "{text}");
    assert!(text.contains("'2024-01-02 10:00:00.003333'"), "{text}");
    assert!(text.contains("'2024-01-02T10:00:00.500+00:00'"), "{text}");
}

#[test]
fn server_maintained_columns_are_not_inserted() {
    use oxbase_mssql::extras;

    let mut table = TableDef::new(ObjectRef::table(None, Some("dbo"), "versioned"));
    let mut id = ColumnDef::new("id", DataType::INT64);
    "bigint".clone_into(&mut id.native_type);
    id.nullable = false;
    id.identity = Some(IdentityDef {
        always: false,
        start: None,
        increment: None,
    });
    let mut name = ColumnDef::new("name", DataType::Text);
    "nvarchar(50)".clone_into(&mut name.native_type);
    let mut rv = ColumnDef::new("rv", DataType::Binary { length: Some(8) });
    "rowversion".clone_into(&mut rv.native_type);
    rv.nullable = false;
    rv.extras.insert(extras::ROW_VERSION, true);
    let mut from = ColumnDef::new("valid_from", DataType::Timestamp { precision: Some(7) });
    "datetime2(7)".clone_into(&mut from.native_type);
    from.nullable = false;
    from.extras.insert(extras::PERIOD, "row_start");
    table.columns = vec![id, name, rv, from];
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", ArrowType::Int64, false),
        Field::new("name", ArrowType::Utf8, true),
        Field::new("rv", ArrowType::Binary, false),
        Field::new(
            "valid_from",
            ArrowType::Timestamp(TimeUnit::Microsecond, None),
            false,
        ),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![7])),
            Arc::new(StringArray::from(vec!["a"])),
            Arc::new(BinaryArray::from(vec![&b"\0\0\0\0\0\0\x07\xd1"[..]])),
            Arc::new(TimestampMicrosecondArray::from(vec![micros(
                2024, 1, 2, 3, 4, 5, 0,
            )])),
        ],
    )
    .unwrap();
    let context = WriteContext {
        dialects: Some(dialects()),
        table: Some(table),
        source_engine: Some(Engine::MsSql),
    };
    let options = SqlDumpOptions {
        include_create: false,
        ..SqlDumpOptions::new(Engine::MsSql)
    };
    let (text, report) = dump_report(Engine::MsSql, options, &context, &[batch]);
    assert!(
        text.ends_with(
            "SET IDENTITY_INSERT [versioned] ON;\n\
             INSERT INTO [versioned] ([id], [name]) VALUES\n(7, N'a');\n\
             SET IDENTITY_INSERT [versioned] OFF;\n"
        ),
        "{text}"
    );
    for column in ["rv", "valid_from"] {
        let note = format!("column {column} left out of the inserts");
        assert!(text.contains(&format!("-- {note}")), "{text}");
        assert!(
            report.warnings.iter().any(|w| w.contains(&note)),
            "{report:?}"
        );
    }
}

#[test]
fn null_typed_columns_dump_null() {
    let schema = Arc::new(Schema::new(vec![
        Field::new("n", ArrowType::Int64, true),
        Field::new("nothing", ArrowType::Null, true),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![1, 2])),
            Arc::new(NullArray::new(2)),
        ],
    )
    .unwrap();
    let options = SqlDumpOptions {
        include_create: false,
        ..SqlDumpOptions::new(Engine::Sqlite)
    };
    let text = dump(Engine::Sqlite, options, &context(), &[batch]);
    assert!(
        text.ends_with(
            "INSERT INTO \"export\" (\"n\", \"nothing\") VALUES\n(1, NULL),\n(2, NULL);\n"
        ),
        "{text}"
    );
}

/// One text column holding `values`.
fn texts(values: &[Option<&str>]) -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![Field::new("txt", ArrowType::Utf8, true)]));
    RecordBatch::try_new(schema, vec![Arc::new(StringArray::from(values.to_vec()))]).unwrap()
}

/// A text that ends a standard string early when a backslash escapes the quote after it.
const BREAKOUT: &str = "\\'); DROP TABLE t; --";

#[test]
fn postgres_dumps_read_backslashes_as_text() {
    // `standard_conforming_strings = off` (a role or database setting) would read `\'` as an
    // escaped quote and run the rest as SQL: the dump turns it on before anything else.
    let text = dump(
        Engine::Postgres,
        SqlDumpOptions {
            include_drop: true,
            ..SqlDumpOptions::new(Engine::Postgres)
        },
        &context(),
        &[texts(&[Some(BREAKOUT), Some("a\\b\\")])],
    );
    let set = text
        .find("SET standard_conforming_strings = on;\n")
        .expect("the setting");
    assert!(set < text.find("DROP TABLE").unwrap(), "{text}");
    assert!(
        text.ends_with(
            "INSERT INTO \"export\" (\"txt\") VALUES\n('\\''); DROP TABLE t; --'),\n('a\\b\\');\n"
        ),
        "{text}"
    );
}

#[test]
fn mysql_dumps_reset_backslash_mode_and_character_set() {
    for engine in [Engine::MySql, Engine::MariaDb] {
        let text = dump(
            engine,
            SqlDumpOptions::new(engine),
            &context(),
            &[texts(&[Some(BREAKOUT), Some("nul \0 end")])],
        );
        let prologue = text.find(MYSQL_PROLOGUE).expect("the prologue");
        assert!(prologue < text.find("CREATE TABLE").unwrap(), "{text}");
        // Backslashes and NULs escaped, as the reset sql_mode reads them; no time zone without
        // a zoned timestamp.
        assert!(
            text.ends_with(&format!(
                "INSERT INTO `export` (`txt`) VALUES\n('\\\\''); DROP TABLE t; --'),\n\
                 ('nul \\0 end');\n{MYSQL_EPILOGUE}"
            )),
            "{engine}:\n{text}"
        );
        assert!(!text.contains("time_zone"), "{engine}:\n{text}");
    }
}

#[test]
fn sqlite_dumps_keep_long_decimals_exact() {
    // decimal(38,10): more digits than a double keeps.
    let values = Decimal128Array::from(vec![
        Some(12_345_678_901_234_567_890_123_456_780_123_456_789_i128),
        Some(-1_000_000_000_i128),
        None,
    ])
    .with_precision_and_scale(38, 10)
    .unwrap();
    let schema = Arc::new(Schema::new(vec![Field::new(
        "amount",
        values.data_type().clone(),
        true,
    )]));
    let batch = RecordBatch::try_new(schema, vec![Arc::new(values)]).unwrap();
    let (text, report) = dump_report(
        Engine::Sqlite,
        SqlDumpOptions::new(Engine::Sqlite),
        &context(),
        &[batch],
    );
    assert!(text.contains("\"amount\" DECIMAL_TEXT(38,10)"), "{text}");
    assert!(
        text.contains("-- column amount: declared DECIMAL_TEXT(38,10)"),
        "{text}"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("DECIMAL_TEXT(38,10)")),
        "{report:?}"
    );
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(&text).unwrap();
    let mut stmt = conn
        .prepare("SELECT \"amount\", typeof(\"amount\") FROM \"export\" ORDER BY rowid")
        .unwrap();
    let rows: Vec<(Option<String>, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            (
                Some("1234567890123456789012345678.0123456789".to_owned()),
                "text".to_owned()
            ),
            (Some("-0.1000000000".to_owned()), "text".to_owned()),
            (None, "null".to_owned()),
        ]
    );
}
