//! Live tests of the M3 MySQL / MariaDB cells against the `mysql` and `mariadb` fixtures:
//!
//! - DDL round trips on `[m3]` scratch tables (`m3_rt_<kind>_<token>`): create → fetch with the
//!   M2 fetchers → compare (empty) → alter → fetch → compare (empty) → drop, for a whole table
//!   and for each child kind (column, index, foreign key, check);
//! - the DML writer on the seed tables of `tests/docker/init/{mysql,mariadb}/05-m3.sql`: insert,
//!   update and delete with bound parameters, read-back (`LAST_INSERT_ID()` / MariaDB
//!   `RETURNING`), generated values, NULL versus the empty string, a row of defaults, and the
//!   affected-row counts the edit service relies on (a no-op update counts 1; a changed or
//!   vanished row counts 0).
//!
//! All are `#[ignore]`d: `cargo test -p oxbase-mysql --test m3_live -- --include-ignored`. Every
//! scenario drops its scratch tables and deletes its rows afterwards, also when it fails.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arrow_array::Array;
use arrow_array::cast::AsArray;
use arrow_array::types::{Decimal128Type, Int32Type, Int64Type, UInt64Type};
use arrow_schema::DataType as ArrowType;
use oxbase_dialect::{
    BacktickQuoting, Comparator, DdlContext, DdlGenerator, DialectRegistry, DmlPlan, DmlTarget,
    Quoting,
};
use oxbase_driver::{CancellationToken, ConnectParams, Connection, ResultSet, Tx, collect};
use oxbase_mysql::{MySqlConnection, MySqlDriver};
use oxbase_types::{
    CheckDef, ColumnDef, ColumnValue, DataType, Engine, Error, Extras, FkAction, ForeignKeyDef,
    GeneratedColumn, IndexColumn, IndexDef, KeyKind, ObjectDef, ObjectKind, ObjectRef,
    PrimaryKeyDef, Rename, RowEdit, RowKey, Script, TableDef, TlsMode, TlsOptions, Value, props,
};

// ---- fixtures ---------------------------------------------------------------------------------

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
            |p| {
                let p = PathBuf::from(p);
                if p.is_absolute() {
                    p
                } else {
                    workspace_root().join(p)
                }
            },
        );
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.parse().expect("fixtures.toml parses")
}

fn get<'a>(t: &'a toml::Table, path: &[&str]) -> &'a toml::Value {
    let (last, tables) = path.split_last().expect("non-empty path");
    let mut table = t;
    for key in tables {
        table = table
            .get(*key)
            .and_then(toml::Value::as_table)
            .unwrap_or_else(|| panic!("fixtures.toml has no table {key}"));
    }
    table
        .get(*last)
        .unwrap_or_else(|| panic!("fixtures.toml has no {}", path.join(".")))
}

fn s(t: &toml::Table, path: &[&str]) -> String {
    get(t, path).as_str().expect("string").to_owned()
}

fn engine_of(key: &str) -> Engine {
    match key {
        "mysql" => Engine::MySql,
        "mariadb" => Engine::MariaDb,
        other => panic!("not a MySQL-family fixture: {other}"),
    }
}

async fn connect(key: &str) -> MySqlConnection {
    let f = fixtures();
    let port = u16::try_from(get(&f, &["engines", key, "port"]).as_integer().unwrap()).unwrap();
    let p = ConnectParams::network(engine_of(key), s(&f, &["engines", key, "host"]))
        .with_port(port)
        .with_user(s(&f, &["engines", key, "user"]))
        .with_password(s(&f, &["engines", key, "password"]))
        .with_database(s(&f, &["m3", key, "database"]))
        .with_tls(TlsOptions::new(TlsMode::Disable));
    MySqlDriver::new()
        .connect_mysql(&p)
        .await
        .unwrap_or_else(|e| panic!("connect {key}: {e}"))
}

fn base36(mut n: u64, width: usize) -> String {
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = vec![b'0'; width];
    for slot in out.iter_mut().rev() {
        *slot = digits[usize::try_from(n % 36).unwrap()];
        n /= 36;
    }
    String::from_utf8(out).unwrap()
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs()
}

/// 10 lowercase base36 characters: 6 for the unix seconds, 4 for the process id.
fn token() -> String {
    format!(
        "{}{}",
        base36(unix_secs(), 6),
        base36(u64::from(std::process::id()) % 36u64.pow(4), 4)
    )
}

/// The unix seconds of a token made by [`token`], if `segment` is one.
fn token_secs(segment: &str) -> Option<u64> {
    if segment.len() != 10
        || !segment
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase())
    {
        return None;
    }
    u64::from_str_radix(&segment[..6], 36).ok()
}

/// What a scenario needs: the connection, the registry, the database and a fresh token.
struct Ctx {
    key: &'static str,
    c: MySqlConnection,
    reg: DialectRegistry,
    db: String,
    token: String,
}

impl Ctx {
    async fn new(key: &'static str) -> Self {
        let mut reg = DialectRegistry::new();
        oxbase_mysql::dialect::register(&mut reg);
        let f = fixtures();
        Self {
            key,
            c: connect(key).await,
            reg,
            db: s(&f, &["m3", key, "database"]),
            token: token(),
        }
    }

    fn engine(&self) -> Engine {
        self.c.engine()
    }

    fn table_ref(&self, name: &str) -> ObjectRef {
        ObjectRef::table(Some(&self.db), None, name)
    }

    fn ddl(&self, kind: ObjectKind) -> Arc<dyn DdlGenerator> {
        self.reg.ddl(self.engine(), kind).expect("DDL generator")
    }

    fn cmp(&self, kind: ObjectKind) -> Arc<dyn Comparator> {
        self.reg
            .comparator(self.engine(), kind)
            .expect("comparator")
    }

    async fn exec(&self, sql: &str, params: &[Value]) -> oxbase_driver::Collected {
        let stream = self
            .c
            .execute(sql, params, CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{}: {sql}: {e}", self.key));
        collect(stream)
            .await
            .unwrap_or_else(|e| panic!("{}: {sql}: {e}", self.key))
    }

    /// Runs every step of `script` in execution order.
    async fn run(&self, script: &Script) {
        assert!(!script.is_empty(), "{}: empty script", self.key);
        for step in script.ordered() {
            assert!(!step.transactional, "MySQL DDL steps commit on their own");
            self.exec(&step.sql, &[]).await;
        }
    }

    async fn load(&self, obj: &ObjectRef) -> oxbase_types::Result<ObjectDef> {
        self.reg
            .fetcher(self.engine(), obj.kind)
            .expect("fetcher")
            .load(&self.c, obj, CancellationToken::new())
            .await
    }

    async fn load_table(&self, name: &str) -> TableDef {
        match self.load(&self.table_ref(name)).await {
            Ok(ObjectDef::Table(t)) => t,
            other => panic!("{}: load {name}: {other:?}", self.key),
        }
    }

    /// Drops `m3_rt_*` tables whose token is older than `stale_after_secs` (left by a killed run).
    async fn drop_stale(&self) {
        let f = fixtures();
        let stale = u64::try_from(get(&f, &["m3", "stale_after_secs"]).as_integer().unwrap())
            .unwrap_or(3600);
        let prefix = s(&f, &["m3", "roundtrip_prefix"]);
        let out = self
            .exec(
                "SELECT CAST(TABLE_NAME AS CHAR) FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME LIKE CONCAT(?, '%')",
                &[Value::Text(self.db.clone()), Value::Text(prefix.clone())],
            )
            .await;
        let now = unix_secs();
        let mut old = Vec::new();
        for rs in &out.result_sets {
            for row in 0..rs.num_rows() {
                let Some(name) = cell(rs, row, 0) else {
                    continue;
                };
                let is_old = name
                    .strip_prefix(&prefix)
                    .into_iter()
                    .flat_map(|rest| rest.split('_'))
                    .filter_map(token_secs)
                    .any(|secs| secs > 1_600_000_000 && secs + stale < now);
                if is_old {
                    old.push(name);
                }
            }
        }
        if !old.is_empty() {
            let mut sql = "DROP TABLE IF EXISTS ".to_owned();
            let names: Vec<String> = old
                .iter()
                .map(|n| BacktickQuoting.qualify(&self.table_ref(n)))
                .collect();
            sql.push_str(&names.join(", "));
            self.exec("SET FOREIGN_KEY_CHECKS = 0", &[]).await;
            self.exec(&sql, &[]).await;
            self.exec("SET FOREIGN_KEY_CHECKS = 1", &[]).await;
        }
    }
}

/// Runs `body` on its own task, then drops `tables` and deletes the edit rows whose code starts
/// with `codes` (over a fresh connection), and re-raises the body's panic.
async fn guarded(
    key: &'static str,
    tables: &[String],
    codes: Option<&str>,
    body: impl Future<Output = ()> + Send + 'static,
) {
    let outcome = tokio::spawn(body).await;
    let t = Ctx::new(key).await;
    t.exec("SET FOREIGN_KEY_CHECKS = 0", &[]).await;
    for name in tables {
        let sql = format!(
            "DROP TABLE IF EXISTS {}",
            BacktickQuoting.qualify(&t.table_ref(name))
        );
        t.exec(&sql, &[]).await;
    }
    t.exec("SET FOREIGN_KEY_CHECKS = 1", &[]).await;
    if let Some(prefix) = codes {
        for table in ["m3_edit_pk", "m3_edit_uq"] {
            let sql = format!(
                "DELETE FROM {} WHERE code LIKE CONCAT(?, '%')",
                BacktickQuoting.qualify(&t.table_ref(table))
            );
            t.exec(&sql, &[Value::Text(prefix.to_owned())]).await;
        }
    }
    t.c.close().await.unwrap();
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

// ---- definitions ------------------------------------------------------------------------------

fn col(name: &str, native: &str, data_type: DataType) -> ColumnDef {
    let mut c = ColumnDef::new(name, data_type);
    c.native_type = native.into();
    c
}

fn literal(text: &str) -> String {
    BacktickQuoting.quote_str(text)
}

fn pk(columns: &[&str]) -> PrimaryKeyDef {
    PrimaryKeyDef {
        name: None,
        columns: columns.iter().map(|c| (*c).to_owned()).collect(),
    }
}

fn index(name: &str, columns: Vec<IndexColumn>, unique: bool) -> IndexDef {
    IndexDef {
        name: name.into(),
        columns,
        unique,
        is_constraint: unique,
        predicate: None,
        extras: Extras::new(),
    }
}

fn varchar(n: u32) -> DataType {
    DataType::VarChar { length: Some(n) }
}

fn decimal(p: u32, s: u32) -> DataType {
    DataType::Decimal {
        precision: Some(p),
        scale: Some(s),
    }
}

/// A table `name(id int PK, <extra columns>)`.
fn simple_table(t: &Ctx, name: &str, extra: Vec<ColumnDef>) -> TableDef {
    let mut def = TableDef::new(t.table_ref(name));
    def.columns
        .push(col("id", "int", DataType::INT32).not_null());
    def.columns.extend(extra);
    def.primary_key = Some(pk(&["id"]));
    def
}

#[track_caller]
fn assert_no_diff(cmp: &dyn Comparator, a: &ObjectDef, b: &ObjectDef, what: &str) {
    let diffs = cmp.diff(a, b);
    if !diffs.is_empty() {
        let text: Vec<String> = diffs.iter().map(ToString::to_string).collect();
        panic!(
            "{what}: {} difference(s):\n  {}\nexpected: {a:#?}\nfetched: {b:#?}",
            diffs.len(),
            text.join("\n  ")
        );
    }
}

/// The value of column `idx` in row `row` as text (`None` for NULL).
fn cell(rs: &ResultSet, mut row: usize, idx: usize) -> Option<String> {
    for batch in &rs.batches {
        if row >= batch.num_rows() {
            row -= batch.num_rows();
            continue;
        }
        let a = batch.column(idx);
        if a.is_null(row) {
            return None;
        }
        return Some(match a.data_type() {
            ArrowType::Int32 => a.as_primitive::<Int32Type>().value(row).to_string(),
            ArrowType::Int64 => a.as_primitive::<Int64Type>().value(row).to_string(),
            ArrowType::UInt64 => a.as_primitive::<UInt64Type>().value(row).to_string(),
            ArrowType::Boolean => a.as_boolean().value(row).to_string(),
            ArrowType::Utf8 => a.as_string::<i32>().value(row).to_owned(),
            ArrowType::Binary => {
                let mut hex = String::new();
                for b in a.as_binary::<i32>().value(row) {
                    let _ = write!(hex, "{b:02x}");
                }
                hex
            }
            ArrowType::Decimal128(_, _) => a.as_primitive::<Decimal128Type>().value_as_string(row),
            other => format!("<{other}>"),
        });
    }
    panic!("row out of range");
}

/// The value of column `name` in row 0.
fn named(rs: &ResultSet, name: &str) -> Option<String> {
    let idx = rs
        .schema
        .index_of(name)
        .unwrap_or_else(|_| panic!("no column {name}"));
    cell(rs, 0, idx)
}

// ---- DDL round trips --------------------------------------------------------------------------

async fn table_round_trip(key: &'static str) {
    let t = Ctx::new(key).await;
    t.drop_stale().await;
    let name = format!("m3_rt_table_{}", t.token);
    let parent = format!("m3_rt_fk_{}_ref", t.token);
    let tables = [name.clone(), parent.clone()];
    guarded(key, &tables, None, async move {
        table_round_trip_body(&t, &name, &parent).await;
    })
    .await;
}

/// The round-trip table: reserved and unicode names, a quoted literal default with quotes, a
/// backslash and unicode, a unicode comment, numeric and boolean literal defaults,
/// `CURRENT_TIMESTAMP` with `ON UPDATE`, a stored generated column, an enum, a unique and a
/// prefix / descending index, a check with a string literal, a foreign key without an index of
/// its own (InnoDB adds one) and a table comment.
fn round_trip_table(t: &Ctx, name: &str, parent: &str) -> TableDef {
    let tok = &t.token;
    let mut def = TableDef::new(t.table_ref(name));
    let mut id = col("id", "int", DataType::INT32).not_null();
    id.auto_increment = true;
    def.columns.push(id);
    let mut order = col("order", "varchar(40)", varchar(40)).not_null();
    order.default = Some(literal("it's \\ \"q\" ü"));
    order.comment = Some("ünï 'c' \\ ok".into());
    def.columns.push(order);
    let mut select = col("select", "decimal(12,2)", decimal(12, 2));
    select.default = Some(literal("0"));
    def.columns.push(select);
    let mut ts = col("ts", "datetime", DataType::Timestamp { precision: None }).not_null();
    ts.default = Some("CURRENT_TIMESTAMP".into());
    ts.extras.insert(
        oxbase_mysql::dialect::ON_UPDATE_EXPRESSION,
        "CURRENT_TIMESTAMP",
    );
    def.columns.push(ts);
    let mut flag = col("flag", "tinyint(1)", DataType::Bool).not_null();
    flag.default = Some(literal("1"));
    def.columns.push(flag);
    let mut total = col("total", "decimal(14,2)", decimal(14, 2));
    total.generated = Some(GeneratedColumn {
        expression: "`select` * 2".into(),
        stored: true,
    });
    def.columns.push(total);
    def.columns.push(col("ref_id", "int", DataType::INT32));
    def.columns.push(col("ünï", "varchar(10)", varchar(10)));
    let mut status = col(
        "status",
        "enum('new','it''s')",
        DataType::Enum {
            labels: vec!["new".into(), "it's".into()],
        },
    )
    .not_null();
    status.default = Some(literal("new"));
    def.columns.push(status);
    def.primary_key = Some(pk(&["id"]));
    def.indexes.push(index(
        &format!("uq_{tok}"),
        vec![IndexColumn::column("order")],
        true,
    ));
    def.indexes.push(index(
        &format!("ix_{tok}"),
        vec![
            IndexColumn {
                length: Some(10),
                ..IndexColumn::column("order")
            },
            IndexColumn {
                descending: true,
                ..IndexColumn::column("select")
            },
        ],
        false,
    ));
    def.checks.push(CheckDef {
        name: Some(format!("chk_{tok}")),
        expression: "`select` >= 0 AND `order` <> 'x'".into(),
        extras: Extras::new(),
    });
    def.foreign_keys.push(ForeignKeyDef {
        name: Some(format!("fk_{tok}")),
        columns: vec!["ref_id".into()],
        ref_table: ObjectRef::table(None, None, parent),
        ref_columns: vec!["id".into()],
        on_delete: FkAction::SetNull,
        on_update: FkAction::Cascade,
        extras: Extras::new(),
    });
    def.comment = Some("Round trip: it's \\ ü".into());
    def
}

async fn table_round_trip_body(t: &Ctx, name: &str, parent: &str) {
    let tok = t.token.clone();
    let (ix, ixr, uq, fk) = (
        format!("ix_{tok}"),
        format!("ixr_{tok}"),
        format!("uq_{tok}"),
        format!("fk_{tok}"),
    );
    let version = t.c.server_version().clone();
    let ctx = DdlContext::new(&version);
    let ddl = t.ddl(ObjectKind::Table);
    let cmp = t.cmp(ObjectKind::Table);

    let parent_def = simple_table(t, parent, vec![]);
    t.run(&ddl.create(&ObjectDef::Table(parent_def), &ctx).unwrap())
        .await;

    // Create, fetch, compare.
    let def = round_trip_table(t, name, parent);
    let script = ddl.create(&ObjectDef::Table(def.clone()), &ctx).unwrap();
    assert_eq!(script.len(), 1);
    t.run(&script).await;
    let fetched = t.load_table(name).await;
    // InnoDB added an index for the foreign key; the comparator knows.
    assert!(
        fetched.indexes.iter().any(|i| i.name == fk),
        "{:?}",
        fetched.indexes
    );
    let as_def = ObjectDef::Table(def.clone());
    let as_fetched = ObjectDef::Table(fetched.clone());
    assert_no_diff(&*cmp, &as_def, &as_fetched, "after create");
    assert_no_diff(&*cmp, &as_fetched, &as_def, "after create, reversed");
    assert!(
        ddl.alter(&as_fetched, &as_def, &ctx.with_table(&fetched))
            .unwrap()
            .is_empty(),
        "nothing to alter after create"
    );

    // Alter the fetched definition the way the designer does.
    let renames = [
        Rename::new(ObjectKind::Column, "ünï", "ünï_2"),
        Rename::new(ObjectKind::Index, ix.as_str(), ixr.as_str()),
    ];
    let mut new = fetched.clone();
    let pos = |t: &TableDef, n: &str| t.columns.iter().position(|c| c.name == n).unwrap();
    let uni = pos(&new, "ünï");
    new.columns[uni].name = "ünï_2".into();
    new.columns[uni].native_type = "varchar(30)".into();
    new.columns[uni].data_type = varchar(30);
    let st = pos(&new, "status");
    new.columns.remove(st);
    let order = pos(&new, "order");
    new.columns[order].default = Some(literal("new 'v' \\"));
    new.columns[order].comment = Some("changed".into());
    new.columns
        .insert(order + 1, col("note", "text", DataType::Text));
    let flag_pos = pos(&new, "flag");
    let flag = new.columns.remove(flag_pos);
    new.columns.insert(0, flag);
    for idx in &mut new.indexes {
        if idx.name == ix {
            idx.name.clone_from(&ixr);
        }
    }
    new.indexes.retain(|i| i.name != uq);
    new.indexes.push(index(
        &format!("ixn_{tok}"),
        vec![IndexColumn {
            length: Some(20),
            ..IndexColumn::column("note")
        }],
        false,
    ));
    new.checks[0].expression = "`select` >= 1".into();
    new.foreign_keys[0].on_delete = FkAction::Cascade;
    new.comment = Some("Changed: ü".into());
    let alter_ctx = DdlContext::new(&version)
        .with_table(&fetched)
        .with_renames(&renames);
    let script = ddl
        .alter(&as_fetched, &ObjectDef::Table(new.clone()), &alter_ctx)
        .unwrap();
    assert!(script.len() >= 3, "{script}");
    assert!(script.is_destructive(), "drops a column");
    t.run(&script).await;
    let after = t.load_table(name).await;
    let names: Vec<&str> = after.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "flag", "id", "order", "note", "select", "ts", "total", "ref_id", "ünï_2"
        ]
    );
    assert_no_diff(
        &*cmp,
        &ObjectDef::Table(new.clone()),
        &ObjectDef::Table(after.clone()),
        "after alter",
    );
    let again = ddl
        .alter(
            &ObjectDef::Table(after.clone()),
            &ObjectDef::Table(new),
            &ctx.with_table(&after),
        )
        .unwrap();
    assert!(again.is_empty(), "nothing left to alter: {again}");

    // Drop both tables.
    t.run(&ddl.drop_object(&t.table_ref(name), &ctx).unwrap())
        .await;
    let gone = t.load(&t.table_ref(name)).await.unwrap_err();
    assert!(matches!(gone, Error::NotFound { .. }), "{gone}");
    t.run(&ddl.drop_object(&t.table_ref(parent), &ctx).unwrap())
        .await;
}

/// Creates `name` from `def` with the table generator.
async fn create_table(t: &Ctx, def: TableDef) {
    let version = t.c.server_version().clone();
    let script = t
        .ddl(ObjectKind::Table)
        .create(&ObjectDef::Table(def), &DdlContext::new(&version))
        .unwrap();
    t.run(&script).await;
}

/// Create → fetch → compare → alter → fetch → compare → drop of one child of `table`.
async fn child_round_trip(t: &Ctx, table: &str, before: &ObjectDef, after: &ObjectDef) {
    let kind = before.kind();
    let version = t.c.server_version().clone();
    let ctx = DdlContext::new(&version);
    let ddl = t.ddl(kind);
    let cmp = t.cmp(kind);
    let table_ref = t.table_ref(table);

    t.run(&ddl.create(before, &ctx).unwrap()).await;
    let fetched = t
        .load(&table_ref.child(kind, before.name()))
        .await
        .unwrap_or_else(|e| panic!("{}: load {kind} {}: {e}", t.key, before.name()));
    assert_no_diff(&*cmp, before, &fetched, "child after create");

    let current = t.load_table(table).await;
    let script = ddl
        .alter(&fetched, after, &ctx.with_table(&current))
        .unwrap();
    t.run(&script).await;
    let refetched = t
        .load(&table_ref.child(kind, after.name()))
        .await
        .unwrap_or_else(|e| panic!("{}: load {kind} {}: {e}", t.key, after.name()));
    assert_no_diff(&*cmp, after, &refetched, "child after alter");

    let current = t.load_table(table).await;
    let script = ddl
        .drop_object(
            &table_ref.child(kind, after.name()),
            &ctx.with_table(&current),
        )
        .unwrap();
    t.run(&script).await;
    let gone = t
        .load(&table_ref.child(kind, after.name()))
        .await
        .unwrap_err();
    assert!(matches!(gone, Error::NotFound { .. }), "{gone}");
}

async fn column_round_trip(key: &'static str) {
    let t = Ctx::new(key).await;
    let name = format!("m3_rt_column_{}", t.token);
    let tables = [name.clone()];
    guarded(key, &tables, None, async move {
        create_table(&t, simple_table(&t, &name, vec![])).await;
        let table = t.table_ref(&name);
        let mut column = col("note`x", "varchar(30)", varchar(30));
        column.default = Some(literal("d'\\ü"));
        column.comment = Some("a 'note'".into());
        let mut changed = column.clone();
        changed.name = "note 2".into();
        changed.native_type = "varchar(60)".into();
        changed.data_type = varchar(60);
        changed.nullable = false;
        changed.default = Some(literal("x"));
        changed.comment = None;
        child_round_trip(
            &t,
            &name,
            &ObjectDef::Column {
                table: table.clone(),
                column,
            },
            &ObjectDef::Column {
                table,
                column: changed,
            },
        )
        .await;
    })
    .await;
}

async fn index_round_trip(key: &'static str) {
    let t = Ctx::new(key).await;
    let name = format!("m3_rt_index_{}", t.token);
    let tables = [name.clone()];
    guarded(key, &tables, None, async move {
        let tok = t.token.clone();
        create_table(
            &t,
            simple_table(
                &t,
                &name,
                vec![
                    col("a", "int", DataType::INT32),
                    col("b", "varchar(20)", varchar(20)),
                ],
            ),
        )
        .await;
        let table = t.table_ref(&name);
        let before = index(
            &format!("ix_{tok}"),
            vec![
                IndexColumn::column("a"),
                IndexColumn {
                    length: Some(5),
                    descending: true,
                    ..IndexColumn::column("b")
                },
            ],
            false,
        );
        let mut after = before.clone();
        after.name = format!("ixr_{tok}");
        after.unique = true;
        after.is_constraint = true;
        after.extras.insert(props::COMMENT, "it's unique");
        child_round_trip(
            &t,
            &name,
            &ObjectDef::Index {
                table: table.clone(),
                index: before,
            },
            &ObjectDef::Index {
                table,
                index: after,
            },
        )
        .await;
    })
    .await;
}

async fn foreign_key_round_trip(key: &'static str) {
    let t = Ctx::new(key).await;
    let name = format!("m3_rt_foreign_key_{}", t.token);
    let parent = format!("m3_rt_fk_{}_ref", t.token);
    let tables = [name.clone(), parent.clone()];
    guarded(key, &tables, None, async move {
        let tok = t.token.clone();
        create_table(&t, simple_table(&t, &parent, vec![])).await;
        create_table(
            &t,
            simple_table(&t, &name, vec![col("ref_id", "int", DataType::INT32)]),
        )
        .await;
        let table = t.table_ref(&name);
        let before = ForeignKeyDef {
            name: Some(format!("fk_{tok}")),
            columns: vec!["ref_id".into()],
            ref_table: t.table_ref(&parent),
            ref_columns: vec!["id".into()],
            on_delete: FkAction::Cascade,
            on_update: FkAction::NoAction,
            extras: Extras::new(),
        };
        let mut after = before.clone();
        after.on_delete = FkAction::SetNull;
        after.on_update = FkAction::Cascade;
        child_round_trip(
            &t,
            &name,
            &ObjectDef::ForeignKey {
                table: table.clone(),
                foreign_key: before,
            },
            &ObjectDef::ForeignKey {
                table,
                foreign_key: after,
            },
        )
        .await;
    })
    .await;
}

async fn check_round_trip(key: &'static str) {
    let t = Ctx::new(key).await;
    let name = format!("m3_rt_check_{}", t.token);
    let tables = [name.clone()];
    guarded(key, &tables, None, async move {
        let tok = t.token.clone();
        create_table(
            &t,
            simple_table(
                &t,
                &name,
                vec![
                    col("qty", "int", DataType::INT32),
                    col("note", "varchar(20)", varchar(20)),
                ],
            ),
        )
        .await;
        let table = t.table_ref(&name);
        let before = CheckDef {
            name: Some(format!("chk_{tok}")),
            expression: "qty >= 0 AND note <> 'it''s'".into(),
            extras: Extras::new(),
        };
        let mut after = before.clone();
        after.expression = "(qty > 0 OR qty IS NULL) AND note IN ('a', 'b')".into();
        child_round_trip(
            &t,
            &name,
            &ObjectDef::Check {
                table: table.clone(),
                check: before,
            },
            &ObjectDef::Check {
                table,
                check: after,
            },
        )
        .await;
    })
    .await;
}

// ---- DML --------------------------------------------------------------------------------------

/// Runs `plan` inside `tx`: the affected-row count as the edit service computes it, and the row
/// read back (from `RETURNING` or the read-back query).
async fn apply(tx: &dyn Tx, plan: &DmlPlan) -> (u64, Option<ResultSet>) {
    let out = collect(
        tx.execute(
            &plan.write.sql,
            &plan.write.params,
            CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("{}: {e}", plan.write.sql)),
    )
    .await
    .unwrap_or_else(|e| panic!("{}: {e}", plan.write.sql));
    if plan.returning {
        let rs = out.result_sets.into_iter().next().expect("RETURNING rows");
        return (u64::try_from(rs.num_rows()).unwrap(), Some(rs));
    }
    let affected = out.rows_affected.iter().sum();
    let Some(read_back) = &plan.read_back else {
        return (affected, None);
    };
    let rows = collect(
        tx.execute(&read_back.sql, &read_back.params, CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap_or_else(|e| panic!("{}: {e}", read_back.sql));
    (affected, rows.result_sets.into_iter().next())
}

/// Plans `edit` on `table` with `key`, reading back `read_back`.
fn plan(t: &Ctx, table: &TableDef, key: &RowKey, edit: &RowEdit, read_back: &[String]) -> DmlPlan {
    t.reg
        .dml_writer(t.engine())
        .unwrap()
        .plan(
            edit,
            &DmlTarget {
                table,
                key,
                version: t.c.server_version(),
                read_back,
            },
        )
        .unwrap_or_else(|e| panic!("plan {edit:?}: {e}"))
}

/// Applies one edit in its own transaction and commits.
async fn apply_one(t: &Ctx, plan: &DmlPlan) -> (u64, Option<ResultSet>) {
    let tx = t.c.begin().await.unwrap();
    let out = apply(&*tx, plan).await;
    tx.commit().await.unwrap();
    out
}

async fn dml_primary_key(key: &'static str) {
    let t = Ctx::new(key).await;
    let codes = format!("t{}-", t.token);
    let prefix = codes.clone();
    guarded(key, &[], Some(&prefix), async move {
        dml_primary_key_body(&t, &codes).await;
    })
    .await;
}

async fn dml_primary_key_body(t: &Ctx, codes: &str) {
    let table = t.load_table("m3_edit_pk").await;
    let key = RowKey::new(table.obj.clone(), KeyKind::Primary, vec!["id".into()]);
    let all: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();

    // Insert: NULL and '' stay apart; the generated key and `total` come back.
    let insert = RowEdit::Insert {
        values: vec![
            ColumnValue::new("code", format!("{codes}1")),
            ColumnValue::new("name", ""),
            ColumnValue::new("amount", Value::Decimal("1.25".into())),
            ColumnValue::new("flag", true),
            ColumnValue::new("doc", Value::Json(serde_json::json!({"k": "ü"}))),
            ColumnValue::new("data", Value::Bytes(vec![0, 1, 0xff])),
            ColumnValue::new("born", Value::Null),
        ],
    };
    let p = plan(t, &table, &key, &insert, &all);
    assert!(p.write.sql.starts_with("INSERT INTO "), "{}", p.write.sql);
    assert_eq!(p.write.params.len(), 7, "every value is bound");
    assert_eq!(p.returning, t.engine() == Engine::MariaDb);
    let (affected, row) = apply_one(t, &p).await;
    assert_eq!(affected, 1);
    let row = row.expect("row read back");
    assert_eq!(row.num_rows(), 1);
    let id = named(&row, "id").expect("generated id");
    assert!(id.parse::<i64>().unwrap() > 3, "new id {id}");
    assert_eq!(named(&row, "total").as_deref(), Some("2.50"));
    assert_eq!(named(&row, "name").as_deref(), Some(""));
    assert_eq!(named(&row, "born"), None);
    assert_eq!(named(&row, "data").as_deref(), Some("0001ff"));
    let id_value = Value::Int(id.parse().unwrap());

    // Update to NULL, and a no-op update still counts one matched row (CLIENT_FOUND_ROWS).
    let update = RowEdit::Update {
        key: vec![id_value.clone()],
        changes: vec![
            ColumnValue::new("name", Value::Null),
            ColumnValue::new("amount", Value::Decimal("2.00".into())),
        ],
    };
    let p = plan(t, &table, &key, &update, &all);
    assert!(!p.returning);
    let (affected, row) = apply_one(t, &p).await;
    assert_eq!(affected, 1);
    let row = row.expect("row read back");
    assert_eq!(named(&row, "name"), None);
    assert_eq!(named(&row, "total").as_deref(), Some("4.00"));
    let (affected, _) = apply_one(t, &p).await;
    assert_eq!(affected, 1, "a no-op update counts the matched row");
    let to_empty = RowEdit::Update {
        key: vec![id_value.clone()],
        changes: vec![ColumnValue::new("name", "")],
    };
    let (_, row) = apply_one(t, &plan(t, &table, &key, &to_empty, &all)).await;
    assert_eq!(named(&row.unwrap(), "name").as_deref(), Some(""));

    // A generated column is never written.
    let generated = RowEdit::Update {
        key: vec![id_value.clone()],
        changes: vec![ColumnValue::new("total", Value::Decimal("1".into()))],
    };
    let err = t
        .reg
        .dml_writer(t.engine())
        .unwrap()
        .plan(
            &generated,
            &DmlTarget {
                table: &table,
                key: &key,
                version: t.c.server_version(),
                read_back: &all,
            },
        )
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput { .. }), "{err}");

    // Another session deletes the row: the update and the delete match nothing (a conflict),
    // and a rollback leaves everything as it was.
    let other = connect(t.key).await;
    collect(
        other
            .execute(
                "DELETE FROM m3_edit_pk WHERE id = ?",
                &[id_value.clone()],
                CancellationToken::new(),
            )
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    other.close().await.unwrap();
    let tx = t.c.begin().await.unwrap();
    let (affected, _) = apply(&*tx, &p).await;
    assert_eq!(affected, 0, "update of a vanished row");
    let delete = RowEdit::Delete {
        key: vec![id_value],
    };
    let (affected, _) = apply(&*tx, &plan(t, &table, &key, &delete, &all)).await;
    assert_eq!(affected, 0, "delete of a vanished row");
    tx.rollback().await.unwrap();

    // Insert with an explicit key, then delete it: exactly one row.
    let insert = RowEdit::Insert {
        values: vec![
            ColumnValue::new("id", 900_000_001),
            ColumnValue::new("code", format!("{codes}2")),
        ],
    };
    let (affected, row) = apply_one(t, &plan(t, &table, &key, &insert, &all)).await;
    assert_eq!(affected, 1);
    assert_eq!(named(&row.unwrap(), "id").as_deref(), Some("900000001"));
    let delete = RowEdit::Delete {
        key: vec![Value::Int(900_000_001)],
    };
    let p = plan(t, &table, &key, &delete, &all);
    assert_eq!(
        p.write.sql,
        "DELETE FROM `oxbase_test`.`m3_edit_pk` WHERE `id` = ?"
    );
    let (affected, row) = apply_one(t, &p).await;
    assert_eq!(affected, 1);
    assert!(row.is_none());
    let (affected, _) = apply_one(t, &p).await;
    assert_eq!(affected, 0);

    // The seed rows are untouched.
    let out = t
        .exec(
            "SELECT COUNT(*) FROM m3_edit_pk WHERE id IN (1, 2, 3) AND code IN ('a', 'b', 'c')",
            &[],
        )
        .await;
    assert_eq!(cell(&out.result_sets[0], 0, 0).as_deref(), Some("3"));
}

async fn dml_unique_key_and_defaults(key: &'static str) {
    let t = Ctx::new(key).await;
    let codes = format!("t{}-", t.token);
    let prefix = codes.clone();
    let scratch = format!("m3_rt_dml_{}", t.token);
    let tables = [scratch.clone()];
    guarded(key, &tables, Some(&prefix), async move {
        // m3_edit_uq: the key is the unique code.
        let table = t.load_table("m3_edit_uq").await;
        let key = RowKey::new(table.obj.clone(), KeyKind::Unique, vec!["code".into()]);
        let all: Vec<String> = vec!["code".into(), "val".into()];
        let insert = RowEdit::Insert {
            values: vec![
                ColumnValue::new("code", format!("{codes}u")),
                ColumnValue::new("val", 5),
            ],
        };
        let (affected, row) = apply_one(&t, &plan(&t, &table, &key, &insert, &all)).await;
        assert_eq!(affected, 1);
        assert_eq!(named(&row.unwrap(), "val").as_deref(), Some("5"));
        // Changing the key reads the row back by its new value.
        let update = RowEdit::Update {
            key: vec![Value::Text(format!("{codes}u"))],
            changes: vec![
                ColumnValue::new("code", format!("{codes}v")),
                ColumnValue::new("val", 6),
            ],
        };
        let (affected, row) = apply_one(&t, &plan(&t, &table, &key, &update, &all)).await;
        assert_eq!(affected, 1);
        let row = row.unwrap();
        assert_eq!(named(&row, "code"), Some(format!("{codes}v")));
        assert_eq!(named(&row, "val").as_deref(), Some("6"));
        // The old key is gone: an update by it is a conflict.
        let (affected, _) = apply_one(&t, &plan(&t, &table, &key, &update, &all)).await;
        assert_eq!(affected, 0);
        // A no-op update on a seed row counts 1 and changes nothing.
        let noop = RowEdit::Update {
            key: vec![Value::Text("x".into())],
            changes: vec![ColumnValue::new("val", 1)],
        };
        let (affected, _) = apply_one(&t, &plan(&t, &table, &key, &noop, &[])).await;
        assert_eq!(affected, 1, "CLIENT_FOUND_ROWS");

        // A row of defaults, read back by LAST_INSERT_ID() (MySQL) or RETURNING (MariaDB).
        let sql = format!(
            "CREATE TABLE {} (id int NOT NULL AUTO_INCREMENT PRIMARY KEY, \
             v varchar(10) NOT NULL DEFAULT 'd', n int NULL)",
            BacktickQuoting.qualify(&t.table_ref(&scratch))
        );
        t.exec(&sql, &[]).await;
        let table = t.load_table(&scratch).await;
        let key = RowKey::new(table.obj.clone(), KeyKind::Primary, vec!["id".into()]);
        let all: Vec<String> = vec!["id".into(), "v".into(), "n".into()];
        let p = plan(&t, &table, &key, &RowEdit::Insert { values: vec![] }, &all);
        assert!(p.write.sql.contains("() VALUES ()"), "{}", p.write.sql);
        let (affected, row) = apply_one(&t, &p).await;
        assert_eq!(affected, 1);
        let row = row.unwrap();
        assert_eq!(named(&row, "id").as_deref(), Some("1"));
        assert_eq!(named(&row, "v").as_deref(), Some("d"));
        assert_eq!(named(&row, "n"), None);
    })
    .await;
}

macro_rules! live {
    ($($name:ident),* $(,)?) => {
        mod mysql {
            $(
                #[tokio::test]
                #[ignore = "needs the mysql fixture (scripts/fixtures-up.sh mysql)"]
                async fn $name() {
                    super::$name("mysql").await;
                }
            )*
        }
        mod mariadb {
            $(
                #[tokio::test]
                #[ignore = "needs the mariadb fixture (scripts/fixtures-up.sh mariadb)"]
                async fn $name() {
                    super::$name("mariadb").await;
                }
            )*
        }
    };
}

live!(
    table_round_trip,
    column_round_trip,
    index_round_trip,
    foreign_key_round_trip,
    check_round_trip,
    dml_primary_key,
    dml_unique_key_and_defaults,
);

#[test]
fn tokens_are_base36_with_a_readable_age() {
    let tok = token();
    assert_eq!(tok.len(), 10);
    let secs = token_secs(&tok).unwrap();
    assert!(secs.abs_diff(unix_secs()) < 5);
    assert_eq!(token_secs("short"), None);
    assert_eq!(base36(35, 2), "0z");
}
