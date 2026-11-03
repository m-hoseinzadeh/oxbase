//! `data_statements` through every SQL engine's grid writer (`docs/m8-contracts.md` §6.4,
//! §12): one stored row as an insert, an update and a delete, values bound (typed for their
//! column: PostgreSQL `numeric` arrives as text and binds as a decimal), previews rendered with
//! the engine's dump literals.

use std::sync::Arc;

use arrow_array::{ArrayRef, BinaryArray, Decimal128Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType as Arrow, Field, Schema};
use oxbase_dialect::{DialectRegistry, inline_params};
use oxbase_diff::RowOp;
use oxbase_diff::data::{InsertSession, data_statements, insert_session, row_key};
use oxbase_types::{
    ColumnDef, DataType, Engine, IdentityDef, ObjectRef, PrimaryKeyDef, ServerVersion, TableDef,
    Value,
};

fn registry() -> DialectRegistry {
    let mut reg = DialectRegistry::new();
    oxbase_pg::dialect::register(&mut reg);
    oxbase_mysql::dialect::register(&mut reg);
    oxbase_sqlite::dialect::register(&mut reg);
    oxbase_mssql::dialect::register(&mut reg);
    reg
}

fn table(engine: Engine) -> TableDef {
    let obj = match engine {
        Engine::MySql | Engine::MariaDb => ObjectRef::table(Some("db"), None, "t"),
        Engine::Sqlite => ObjectRef::table(Some("main"), None, "t"),
        _ => ObjectRef::table(Some("db"), Some("s"), "t"),
    };
    let mut t = TableDef::new(obj);
    t.columns
        .push(ColumnDef::new("id", DataType::INT64).not_null());
    t.columns.push(ColumnDef::new("name", DataType::Text));
    t.columns.push(ColumnDef::new(
        "amount",
        DataType::Decimal {
            precision: Some(38),
            scale: Some(10),
        },
    ));
    t.columns.push(ColumnDef::new(
        "payload",
        DataType::VarBinary { length: Some(16) },
    ));
    t.columns.push(ColumnDef::new("note", DataType::Text));
    t.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec!["id".into()],
    });
    t
}

/// One stored row: key, then the compared columns. `amount` as text (PostgreSQL) or as an
/// Arrow decimal (the other drivers).
fn rows(decimal_as_text: bool) -> RecordBatch {
    let amount: (Arrow, ArrayRef) = if decimal_as_text {
        (
            Arrow::Utf8,
            Arc::new(StringArray::from(vec!["12.5000000000"])),
        )
    } else {
        (
            Arrow::Decimal128(38, 10),
            Arc::new(
                Decimal128Array::from(vec![125_000_000_000_i128])
                    .with_precision_and_scale(38, 10)
                    .unwrap(),
            ),
        )
    };
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", Arrow::Int64, true),
        Field::new("name", Arrow::Utf8, true),
        Field::new("amount", amount.0, true),
        Field::new("payload", Arrow::Binary, true),
        Field::new("note", Arrow::Utf8, true),
    ]));
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![7])),
            Arc::new(StringArray::from(vec!["it's"])),
            amount.1,
            Arc::new(BinaryArray::from(vec![&[0_u8, 255][..]])),
            Arc::new(StringArray::from(vec![None::<&str>])),
        ],
    )
    .unwrap()
}

fn version(engine: Engine) -> ServerVersion {
    match engine {
        Engine::Postgres => ServerVersion::new(18, 6, 0),
        Engine::MySql => ServerVersion::new(8, 4, 13),
        Engine::MariaDb => ServerVersion::new(11, 8, 9),
        Engine::MsSql => ServerVersion::new(17, 0, 0),
        _ => ServerVersion::new(3, 50, 0),
    }
}

#[test]
fn every_writer_renders_inserts_updates_and_deletes() {
    let reg = registry();
    for engine in [
        Engine::Postgres,
        Engine::MySql,
        Engine::MariaDb,
        Engine::Sqlite,
        Engine::MsSql,
    ] {
        let def = table(engine);
        let key = row_key(&def, &["id".into()], None);
        let batch = rows(engine == Engine::Postgres);
        let quoting = reg.quoting(engine).unwrap();
        let mut previews = Vec::new();
        for op in [RowOp::Insert, RowOp::Update, RowOp::Delete] {
            let stored = if op == RowOp::Delete {
                batch.project(&[0]).unwrap()
            } else {
                batch.clone()
            };
            let statements =
                data_statements(&reg, engine, &version(engine), &def, &key, op, &stored).unwrap();
            assert_eq!(statements.len(), 1, "{engine:?} {op:?}");
            let st = &statements[0];
            // Every value is bound, never spliced into the SQL.
            assert!(
                !st.sql.contains("it's") && !st.sql.contains("12.5"),
                "{}",
                st.sql
            );
            if op != RowOp::Delete {
                assert!(
                    st.params.contains(&Value::Text("it's".into())),
                    "{engine:?} {op:?}: {:?}",
                    st.params
                );
                assert!(
                    st.params
                        .iter()
                        .any(|v| matches!(v, Value::Decimal(d) if d.starts_with("12.5"))),
                    "{engine:?} {op:?}: {:?}",
                    st.params
                );
                assert!(
                    st.params.contains(&Value::Bytes(vec![0, 255])),
                    "{engine:?} {op:?}: {:?}",
                    st.params
                );
            }
            assert!(st.params.contains(&Value::Int(7)), "{:?}", st.params);
            previews.push(inline_params(quoting.as_ref(), st).unwrap());
        }
        let [insert, update, delete] = &previews[..] else {
            unreachable!()
        };
        assert!(insert.contains("INSERT INTO"), "{engine:?}: {insert}");
        assert!(update.contains("UPDATE"), "{engine:?}: {update}");
        assert!(delete.contains("DELETE FROM"), "{engine:?}: {delete}");
        assert!(insert.contains("12.5"), "{engine:?}: {insert}");
        for text in &previews {
            assert!(!text.contains('?'), "{engine:?}: {text}");
        }
    }
}

#[test]
fn postgres_statements_are_pinned() {
    let reg = registry();
    let def = table(Engine::Postgres);
    let key = row_key(&def, &["id".into()], None);
    let batch = rows(true);
    let quoting = reg.quoting(Engine::Postgres).unwrap();
    let render = |op: RowOp, stored: &RecordBatch| {
        let st = data_statements(
            &reg,
            Engine::Postgres,
            &version(Engine::Postgres),
            &def,
            &key,
            op,
            stored,
        )
        .unwrap()
        .remove(0);
        (
            st.sql.clone(),
            inline_params(quoting.as_ref(), &st).unwrap(),
        )
    };
    let (sql, text) = render(RowOp::Insert, &batch);
    assert_eq!(
        sql,
        "INSERT INTO \"s\".\"t\" (\"id\", \"name\", \"amount\", \"payload\", \"note\") VALUES \
         ($1, $2, $3, $4, $5)"
    );
    assert_eq!(
        text,
        "INSERT INTO \"s\".\"t\" (\"id\", \"name\", \"amount\", \"payload\", \"note\") VALUES \
         (7, 'it''s', 12.5000000000, '\\x00ff'::bytea, NULL)"
    );
    let (sql, _) = render(RowOp::Update, &batch);
    assert_eq!(
        sql,
        "UPDATE \"s\".\"t\" SET \"name\" = $1, \"amount\" = $2, \"payload\" = $3, \"note\" = $4 \
         WHERE \"id\" = $5"
    );
    let (sql, text) = render(RowOp::Delete, &batch.project(&[0]).unwrap());
    assert_eq!(sql, "DELETE FROM \"s\".\"t\" WHERE \"id\" = $1");
    assert_eq!(text, "DELETE FROM \"s\".\"t\" WHERE \"id\" = 7");
}

const ENGINES: [Engine; 5] = [
    Engine::Postgres,
    Engine::MySql,
    Engine::MariaDb,
    Engine::Sqlite,
    Engine::MsSql,
];

fn object(engine: Engine, name: &str) -> ObjectRef {
    match engine {
        Engine::MySql | Engine::MariaDb => ObjectRef::table(Some("db"), None, name),
        Engine::Sqlite => ObjectRef::table(Some("main"), None, name),
        _ => ObjectRef::table(Some("db"), Some("s"), name),
    }
}

/// `(id, v)` with `id` generated by the engine: `GENERATED ALWAYS` identity (PostgreSQL),
/// `IDENTITY` (SQL Server), auto-increment (the others).
fn identity_table(engine: Engine) -> TableDef {
    let mut t = TableDef::new(object(engine, "t"));
    let mut id = ColumnDef::new("id", DataType::INT64).not_null();
    id.auto_increment = true;
    if matches!(engine, Engine::Postgres | Engine::MsSql) {
        id.identity = Some(IdentityDef {
            always: engine == Engine::Postgres,
            start: Some(1),
            increment: Some(1),
        });
    }
    t.columns.push(id);
    t.columns.push(ColumnDef::new("v", DataType::Text));
    t.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec!["id".into()],
    });
    t
}

fn id_rows() -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", Arrow::Int64, true),
        Field::new("v", Arrow::Utf8, true),
    ]));
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![3])),
            Arc::new(StringArray::from(vec!["c"])),
        ],
    )
    .unwrap()
}

#[test]
fn identity_keys_are_written_with_the_engines_identity_insert() {
    let reg = registry();
    for engine in ENGINES {
        let def = identity_table(engine);
        let key = row_key(&def, &["id".into()], None);
        let version = version(engine);
        let insert = data_statements(
            &reg,
            engine,
            &version,
            &def,
            &key,
            RowOp::Insert,
            &id_rows(),
        )
        .unwrap_or_else(|e| panic!("{engine:?}: {e}"))
        .remove(0);
        // The source's id is written as a value.
        assert_eq!(insert.params, [Value::Int(3), "c".into()], "{engine:?}");
        let update = data_statements(
            &reg,
            engine,
            &version,
            &def,
            &key,
            RowOp::Update,
            &id_rows(),
        )
        .unwrap_or_else(|e| panic!("{engine:?}: {e}"))
        .remove(0);
        assert!(update.sql.contains("UPDATE "), "{}", update.sql);
        assert!(!update.sql.contains("OVERRIDING"), "{}", update.sql);
        let session = insert_session(&reg, engine, &def, &["id".into(), "v".into()]).unwrap();
        let sql = |list: &[oxbase_types::BoundStatement]| -> Vec<String> {
            list.iter().map(|s| s.sql.clone()).collect()
        };
        match engine {
            Engine::Postgres => {
                assert_eq!(
                    insert.sql,
                    "INSERT INTO \"s\".\"t\" (\"id\", \"v\") OVERRIDING SYSTEM VALUE VALUES ($1, \
                     $2)"
                );
                assert_eq!(
                    session.before,
                    Vec::<oxbase_types::BoundStatement>::new(),
                    "{session:?}"
                );
                // The resync (if the bulk loader has one) moves the sequence past the ids.
                assert!(
                    session.after.iter().all(|s| s.sql.contains("setval")),
                    "{session:?}"
                );
            }
            Engine::MsSql => {
                // The writer's batch (key table, OUTPUT) with the identity column listed.
                assert!(
                    insert.sql.contains("INSERT INTO [s].[t] ([id], [v])"),
                    "{}",
                    insert.sql
                );
                assert_eq!(sql(&session.before), ["SET IDENTITY_INSERT [s].[t] ON"]);
                assert_eq!(
                    sql(&session.after)[..1],
                    ["SET IDENTITY_INSERT [s].[t] OFF".to_owned()]
                );
            }
            _ => {
                assert!(insert.sql.contains("INSERT INTO "), "{}", insert.sql);
                assert!(!insert.sql.contains("OVERRIDING"), "{}", insert.sql);
                assert_eq!(
                    session.before,
                    Vec::<oxbase_types::BoundStatement>::new(),
                    "{engine:?}: {session:?}"
                );
            }
        }
        // Without an identity column nothing wraps the inserts.
        let plain = table(engine);
        let none = insert_session(&reg, engine, &plain, &["id".into(), "name".into()]).unwrap();
        assert_eq!(none, InsertSession::default(), "{engine:?}");
    }
}

/// Names with quote characters and spaces, values that look like placeholders or escapes: the
/// SQL quotes every name with the engine's quoting, binds every value, and the preview renders
/// each value as the engine's dump literal (nothing inside a literal is taken for a placeholder).
#[test]
fn awkward_names_and_values_are_quoted_and_bound() {
    let reg = registry();
    let names = ["k\"ey", "va l", "x]`y"];
    let values = ["it's", "back\\slash", "?", "$1", "@P1"];
    for engine in ENGINES {
        let quoting = reg.quoting(engine).unwrap();
        let mut def = TableDef::new(object(engine, "we\"ird `t]"));
        def.columns
            .push(ColumnDef::new(names[0], DataType::INT64).not_null());
        def.columns.push(ColumnDef::new(names[1], DataType::Text));
        def.columns.push(ColumnDef::new(names[2], DataType::Text));
        def.primary_key = Some(PrimaryKeyDef {
            name: None,
            columns: vec![names[0].into()],
        });
        let key = row_key(&def, &[names[0].into()], None);
        let schema = Arc::new(Schema::new(vec![
            Field::new(names[0], Arrow::Int64, true),
            Field::new(names[1], Arrow::Utf8, true),
            Field::new(names[2], Arrow::Utf8, true),
        ]));
        for (i, pair) in values.windows(2).enumerate() {
            let batch = RecordBatch::try_new(
                Arc::clone(&schema),
                vec![
                    Arc::new(Int64Array::from(vec![i64::try_from(i).unwrap()])),
                    Arc::new(StringArray::from(vec![pair[0]])),
                    Arc::new(StringArray::from(vec![pair[1]])),
                ],
            )
            .unwrap();
            for op in [RowOp::Insert, RowOp::Update, RowOp::Delete] {
                let stored = if op == RowOp::Delete {
                    batch.project(&[0]).unwrap()
                } else {
                    batch.clone()
                };
                let st = data_statements(&reg, engine, &version(engine), &def, &key, op, &stored)
                    .unwrap_or_else(|e| panic!("{engine:?} {op:?}: {e}"))
                    .remove(0);
                assert!(
                    st.sql.contains(&quoting.qualify(&def.obj)),
                    "{engine:?}: {}",
                    st.sql
                );
                assert!(
                    st.sql.contains(&quoting.quote_ident(names[0])),
                    "{engine:?}: {}",
                    st.sql
                );
                let text = inline_params(quoting.as_ref(), &st)
                    .unwrap_or_else(|e| panic!("{engine:?} {op:?}: {e}: {}", st.sql));
                if op == RowOp::Delete {
                    assert!(!st.params.iter().any(|v| matches!(v, Value::Text(_))));
                    continue;
                }
                for (column, value) in names[1..].iter().zip(pair) {
                    assert!(
                        st.sql.contains(&quoting.quote_ident(column)),
                        "{engine:?}: {}",
                        st.sql
                    );
                    assert!(
                        st.params.contains(&Value::Text((*value).to_owned())),
                        "{engine:?}: {:?}",
                        st.params
                    );
                    let literal = quoting.dump_literal(&Value::Text((*value).to_owned())).sql;
                    assert!(
                        text.contains(&literal),
                        "{engine:?} {op:?}: {literal} not in {text}"
                    );
                }
            }
        }
    }
}
