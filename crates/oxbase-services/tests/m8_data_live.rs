//! M8 data sync through `DataSyncService` (`docs/m8-contracts.md` §6, §12), per SQL engine:
//!
//! - the `[m8]` data pairs (`m8_data` 2 / 5 / 2, `m8_big` 100 000 rows 1 / 1 / 1 with chunks of
//!   10 000, `m8_composite` 0 / 1 / 0, `m8_nokey` refused) built as scratch tables
//!   (`[m8] scratch_prefix`), compared with events, previewed (inserts / updates; with
//!   deletes: destructive), refused without the delete confirmation, applied in batches with
//!   deletes confirmed, and compared again: in sync;
//! - where the engine registers a `ChunkHasher`, the equal 10 000-row chunks of `m8_big` are
//!   skipped by digest;
//! - a cancelled compare ends `Cancelled` within 2 s; a forgotten compare is gone;
//! - when the conformance seeds exist (`m8_src` / `m8_tgt`), their data tables compare with the
//!   `[m8] data_expected` counts (read only); missing seeds are "pending" unless
//!   `OXBASE_SERVICES_REQUIRE_M8=1`.
//!
//! PostgreSQL, MySQL, MariaDB and SQL Server use the Docker fixtures (`#[ignore]`d:
//! `cargo test -p oxbase-services --test m8_data_live -- --include-ignored`); SQLite runs on two
//! temporary files and needs nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_services::sync::{
    DataApplyEvent, DataApplyOptions, DataApplyReport, DataApplyRequest, DataCompareEvent,
    DataCompareOptions, DataCompareRequest, DataCompareResult, DataScriptOptions,
    DataScriptRequest, DataSyncService, TableCompareStatus, TableDataDiff, TablePair,
};
use oxbase_services::tree::TreeService;
use oxbase_services::{
    CancellationToken, ConnectionManager, ProvidedSecrets, SessionId, default_dialects,
    default_drivers,
};
use oxbase_store::{
    ConnectionProfile, MemorySecretStore, ProfileAddress, Secret, SecretRef, SecretSlot,
    SecretStore, TlsMode, TlsSettings,
};
use oxbase_types::{Engine, ErrorCategory, ObjectRef};
use tokio::sync::mpsc;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn fixtures() -> toml::Value {
    let path = std::env::var_os("OXBASE_FIXTURES")
        .map(PathBuf::from)
        .map_or_else(|| root().join("tests/fixtures.toml"), |p| root().join(p));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    toml::Value::Table(text.parse().expect("fixtures.toml parses"))
}

fn s(t: &toml::Value, key: &str) -> String {
    t[key]
        .as_str()
        .unwrap_or_else(|| panic!("fixtures.toml: missing {key}"))
        .to_owned()
}

fn report(key: &str, line: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr().lock(), "[m8 data {key}] {line}");
}

fn require_m8() -> bool {
    std::env::var("OXBASE_SERVICES_REQUIRE_M8").is_ok_and(|v| v == "1")
}

/// How one engine spells the scratch tables and their literals.
struct Spell {
    key: &'static str,
    engine: Engine,
    /// Catalog and schema of the scratch tables.
    catalog: Option<&'static str>,
    schema: Option<&'static str>,
    text: &'static str,
    long_text: &'static str,
    decimal: &'static str,
    bytes: &'static str,
    zoned: &'static str,
    /// Table options after `CREATE TABLE (...)`.
    table_suffix: &'static str,
    /// Prefix of a Unicode string literal.
    n: &'static str,
    bytes_lit: fn(&str) -> String,
    /// The fixed instant 2026-03-01 10:00:00.123456+05:30 plus `plus` seconds.
    zoned_lit: fn(u32) -> String,
    /// `SELECT n FROM ... ` producing 1..=100000 as column `n`.
    numbers: &'static str,
}

const DIGITS: &str = "(SELECT 0 AS d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 \
                      UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT \
                      7 UNION ALL SELECT 8 UNION ALL SELECT 9)";

fn digits_numbers() -> &'static str {
    static TEXT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TEXT.get_or_init(|| {
        format!(
            "SELECT a.d + 10 * b.d + 100 * c.d + 1000 * e.d + 10000 * f.d + 1 AS n FROM {DIGITS} \
             a, {DIGITS} b, {DIGITS} c, {DIGITS} e, {DIGITS} f"
        )
    })
}

fn spell(key: &'static str) -> Spell {
    match key {
        "postgres" => Spell {
            key,
            engine: Engine::Postgres,
            catalog: Some("oxbase_test"),
            schema: Some("public"),
            text: "varchar(100)",
            long_text: "text",
            decimal: "numeric(38,10)",
            bytes: "bytea",
            zoned: "timestamptz",
            table_suffix: "",
            n: "",
            bytes_lit: |hex| format!("'\\x{hex}'::bytea"),
            zoned_lit: |plus| format!("TIMESTAMPTZ '2026-03-01 10:00:{plus:02}.123456+05:30'"),
            numbers: "SELECT i AS n FROM generate_series(1, 100000) AS g(i)",
        },
        "mysql" | "mariadb" => Spell {
            key,
            engine: if key == "mysql" {
                Engine::MySql
            } else {
                Engine::MariaDb
            },
            catalog: Some("oxbase_test"),
            schema: None,
            text: "varchar(100)",
            long_text: "text",
            decimal: "decimal(38,10)",
            bytes: "varbinary(16)",
            zoned: "datetime(6)",
            table_suffix: " CHARACTER SET utf8mb4",
            n: "",
            bytes_lit: |hex| format!("X'{hex}'"),
            // datetime(6) holds the UTC instant.
            zoned_lit: |plus| format!("'2026-03-01 04:30:{plus:02}.123456'"),
            numbers: "",
        },
        "mssql" => Spell {
            key,
            engine: Engine::MsSql,
            catalog: Some("oxbase_test"),
            schema: Some("dbo"),
            text: "nvarchar(100)",
            long_text: "nvarchar(max)",
            decimal: "decimal(38,10)",
            bytes: "varbinary(16)",
            zoned: "datetimeoffset(7)",
            table_suffix: "",
            n: "N",
            bytes_lit: |hex| format!("0x{hex}"),
            zoned_lit: |plus| format!("'2026-03-01 10:00:{plus:02}.1234560 +05:30'"),
            numbers: "",
        },
        "sqlite" => Spell {
            key,
            engine: Engine::Sqlite,
            catalog: Some("main"),
            schema: None,
            text: "varchar(100)",
            long_text: "text",
            decimal: "text",
            bytes: "blob",
            zoned: "text",
            table_suffix: "",
            n: "",
            bytes_lit: |hex| format!("X'{hex}'"),
            zoned_lit: |plus| format!("'2026-03-01T10:00:{plus:02}.123456+05:30'"),
            numbers: "WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < \
                      100000) SELECT n FROM g",
        },
        other => panic!("no engine {other}"),
    }
}

impl Spell {
    fn numbers(&self) -> &str {
        if self.numbers.is_empty() {
            digits_numbers()
        } else {
            self.numbers
        }
    }

    fn table(&self, name: &str) -> ObjectRef {
        ObjectRef::table(self.catalog, self.schema, name)
    }

    /// The qualified name as SQL text.
    fn q(&self, name: &str) -> String {
        let quote = |part: &str| match self.engine {
            Engine::MySql | Engine::MariaDb => format!("`{part}`"),
            Engine::MsSql => format!("[{part}]"),
            _ => format!("\"{part}\""),
        };
        match (self.engine, self.schema) {
            (Engine::Sqlite, _) => quote(name),
            (_, Some(schema)) => format!("{}.{}", quote(schema), quote(name)),
            (_, None) => format!(
                "{}.{}",
                quote(self.catalog.unwrap_or_default()),
                quote(name)
            ),
        }
    }

    fn create(&self, p: &str) -> Vec<String> {
        let (t, lt, dec, b, z, sfx) = (
            self.text,
            self.long_text,
            self.decimal,
            self.bytes,
            self.zoned,
            self.table_suffix,
        );
        vec![
            format!(
                "CREATE TABLE {} (id bigint NOT NULL PRIMARY KEY, name {t} NOT NULL, amount \
                 {dec}, payload {b}, happened_at {z}, note {lt}){sfx}",
                self.q(&format!("{p}_data"))
            ),
            format!(
                "CREATE TABLE {} (id bigint NOT NULL PRIMARY KEY, val int, label varchar(20))\
                 {sfx}",
                self.q(&format!("{p}_big"))
            ),
            format!(
                "CREATE TABLE {} (tenant int NOT NULL, code varchar(20) NOT NULL, qty int, \
                 PRIMARY KEY (tenant, code)){sfx}",
                self.q(&format!("{p}_composite"))
            ),
            format!(
                "CREATE TABLE {} (a int, b {lt}){sfx}",
                self.q(&format!("{p}_nokey"))
            ),
        ]
    }

    fn data_row(&self, i: u32) -> String {
        let n = self.n;
        let name = match i % 3 {
            0 => format!("grüße {i}"),
            1 => format!("日本 {i}"),
            _ => format!("😀 {i}"),
        };
        let amount = if i % 4 == 0 {
            "-1234567890123456789012345678.0123456789"
        } else {
            "1234567890123456789012345678.0123456789"
        };
        let amount = if self.engine == Engine::Sqlite {
            format!("'{amount}'")
        } else {
            amount.to_owned()
        };
        let payload = if i % 2 == 0 {
            (self.bytes_lit)("00ff10")
        } else {
            "NULL".to_owned()
        };
        let note = if i == 10 {
            format!("{n}'y'")
        } else {
            "NULL".to_owned()
        };
        format!(
            "({i}, {n}'{name}', {amount}, {payload}, {}, {note})",
            (self.zoned_lit)(0)
        )
    }

    /// Statements that fill the pair's tables (`upto` = last `m8_data` id) and, for the
    /// target, make the designed differences.
    fn fill(&self, p: &str, target: bool) -> Vec<String> {
        let n = self.n;
        let upto = if target { 18 } else { 20 };
        let rows: Vec<String> = (1..=upto).map(|i| self.data_row(i)).collect();
        let data = self.q(&format!("{p}_data"));
        let big = self.q(&format!("{p}_big"));
        let composite = self.q(&format!("{p}_composite"));
        let label = match self.engine {
            Engine::Postgres | Engine::Sqlite => "'row-' || n",
            _ => "CONCAT('row-', n)",
        };
        let mut out = vec![
            format!(
                "INSERT INTO {data} (id, name, amount, payload, happened_at, note) VALUES {}",
                rows.join(", ")
            ),
            format!(
                "INSERT INTO {big} (id, val, label) SELECT n, n % 1000, {label} FROM ({}) AS \
                 numbers",
                self.numbers()
            ),
            format!(
                "INSERT INTO {composite} (tenant, code, qty) VALUES (1, 'a', 1), (1, 'b', 2), \
                 (2, 'a', 3), (2, 'b', 4), (3, 'a', 5), (3, 'b', 6)"
            ),
            format!(
                "INSERT INTO {} (a, b) VALUES (1, {n}'x')",
                self.q(&format!("{p}_nokey"))
            ),
        ];
        if target {
            let minus = if self.engine == Engine::Sqlite {
                "'-1.5'"
            } else {
                "-1.5"
            };
            out.extend([
                format!("UPDATE {data} SET name = {n}'changed' WHERE id = 3"),
                format!("UPDATE {data} SET amount = {minus} WHERE id = 5"),
                format!(
                    "UPDATE {data} SET happened_at = {} WHERE id = 7",
                    (self.zoned_lit)(1)
                ),
                format!("UPDATE {data} SET note = {n}'x' WHERE id = 9"),
                format!("UPDATE {data} SET note = NULL WHERE id = 10"),
                format!(
                    "INSERT INTO {data} (id, name, amount, payload, happened_at, note) VALUES \
                     (21, {n}'extra', 0, NULL, NULL, NULL), (22, {n}'extra 2', 1, {}, NULL, \
                     {n}'')",
                    (self.bytes_lit)("")
                ),
                format!("UPDATE {big} SET val = 9999 WHERE id = 500"),
                format!("DELETE FROM {big} WHERE id = 50000"),
                format!("INSERT INTO {big} (id, val, label) VALUES (100001, 1, 'row-100001')"),
                format!("UPDATE {composite} SET qty = 40 WHERE tenant = 2 AND code = 'b'"),
            ]);
        }
        out
    }

    fn drop_all(&self, p: &str) -> Vec<String> {
        ["data", "big", "composite", "nokey"]
            .iter()
            .map(|t| format!("DROP TABLE IF EXISTS {}", self.q(&format!("{p}_{t}"))))
            .collect()
    }
}

/// Base sessions on the fixture (the same one twice for servers; two SQLite files).
struct Live {
    spell: Spell,
    manager: Arc<ConnectionManager>,
    service: DataSyncService,
    source: SessionId,
    target: SessionId,
    _dir: tempfile::TempDir,
}

const SOURCE: &str = "m8_t_dsync_s";
const TARGET: &str = "m8_t_dsync_t";

impl Live {
    async fn connect(key: &'static str) -> Self {
        let doc = fixtures();
        let spell = spell(key);
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemorySecretStore::new());
        let manager = Arc::new(ConnectionManager::new(
            default_drivers(),
            secrets.clone(),
            Arc::new(RusshTunnelProvider::new()),
        ));
        let open = |profile: ConnectionProfile| {
            let manager = Arc::clone(&manager);
            async move {
                manager
                    .open(&profile, &ProvidedSecrets::none(), CancellationToken::new())
                    .await
                    .unwrap_or_else(|e| panic!("{key}: connect failed: {e}"))
                    .id()
            }
        };
        let (source, target) = if spell.engine == Engine::Sqlite {
            let file = |name: &str| {
                ConnectionProfile::new(
                    format!("m8 data {name}"),
                    Engine::Sqlite,
                    ProfileAddress::File {
                        path: dir.path().join(format!("{name}.sqlite")),
                    },
                )
            };
            (open(file("src")).await, open(file("tgt")).await)
        } else {
            let e = &doc["engines"][key];
            let mut p = ConnectionProfile::new(
                format!("m8 data {key}"),
                spell.engine,
                ProfileAddress::Network {
                    host: s(e, "host"),
                    port: e["port"].as_integer().and_then(|p| u16::try_from(p).ok()),
                },
            );
            p.user = Some(s(e, "user"));
            p.database = Some(s(e, "database"));
            let secret = p.secret_key(SecretSlot::Password);
            secrets
                .set(&secret, &Secret::new(s(e, "password")))
                .await
                .unwrap();
            p.password = Some(SecretRef::Keychain { key: secret });
            p.tls = TlsSettings {
                mode: TlsMode::Disable,
                ..TlsSettings::default()
            };
            p.connect_timeout_secs = Some(15);
            let base = open(p).await;
            (base, base)
        };
        let dialects = Arc::new(default_dialects());
        let tree = Arc::new(TreeService::new(
            Arc::clone(&manager),
            Arc::clone(&dialects),
        ));
        let service = DataSyncService::new(Arc::clone(&manager), tree, dialects);
        Self {
            spell,
            manager,
            service,
            source,
            target,
            _dir: dir,
        }
    }

    async fn exec(&self, session: SessionId, sql: &str) {
        let handle = self.manager.get(session).expect("open session");
        let _guard = handle.exclusive().await;
        let stream = handle
            .connection()
            .execute(sql, &[], CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{}: {e}: {sql}", self.spell.key));
        oxbase_driver::collect(stream)
            .await
            .unwrap_or_else(|e| panic!("{}: {e}: {sql}", self.spell.key));
    }

    async fn try_exec(&self, session: SessionId, sql: &str) {
        let Some(handle) = self.manager.get(session) else {
            return;
        };
        let _guard = handle.exclusive().await;
        if let Ok(stream) = handle
            .connection()
            .execute(sql, &[], CancellationToken::new())
            .await
        {
            let _ = oxbase_driver::collect(stream).await;
        }
    }

    async fn setup(&self) {
        for (session, p, target) in [(self.source, SOURCE, false), (self.target, TARGET, true)] {
            for sql in self.spell.drop_all(p) {
                self.try_exec(session, &sql).await;
            }
            for sql in self.spell.create(p) {
                self.exec(session, &sql).await;
            }
            for sql in self.spell.fill(p, target) {
                self.exec(session, &sql).await;
            }
        }
    }

    async fn cleanup(&self) {
        for (session, p) in [(self.source, SOURCE), (self.target, TARGET)] {
            for sql in self.spell.drop_all(p) {
                self.try_exec(session, &sql).await;
            }
        }
    }

    fn pairs(&self) -> Vec<TablePair> {
        ["data", "big", "composite", "nokey"]
            .iter()
            .map(|t| TablePair {
                source: self.spell.table(&format!("{SOURCE}_{t}")),
                target: self.spell.table(&format!("{TARGET}_{t}")),
                key: None,
                columns: None,
            })
            .collect()
    }

    fn request(&self, pairs: Vec<TablePair>, options: DataCompareOptions) -> DataCompareRequest {
        DataCompareRequest {
            source_session: self.source,
            target_session: self.target,
            pairs,
            options,
        }
    }

    async fn compare(&self, request: DataCompareRequest) -> (Vec<DataCompareEvent>, Duration) {
        let started = Instant::now();
        let handle = self
            .service
            .start_compare(request)
            .await
            .unwrap_or_else(|e| panic!("{}: start_compare: {e}", self.spell.key));
        let events = drain(handle.events, DataCompareEvent::is_terminal).await;
        (events, started.elapsed())
    }
}

/// Every event up to and including the terminal one (5 minutes at most).
async fn drain<E: std::fmt::Debug>(mut rx: mpsc::Receiver<E>, terminal: fn(&E) -> bool) -> Vec<E> {
    let mut out = Vec::new();
    let deadline = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => panic!("no terminal event after 300 s: {out:?}"),
            event = rx.recv() => match event {
                Some(e) => {
                    let end = terminal(&e);
                    out.push(e);
                    if end {
                        return out;
                    }
                }
                None => panic!("the event stream ended without a terminal event: {out:?}"),
            },
        }
    }
}

fn finished(key: &str, events: &[DataCompareEvent]) -> DataCompareResult {
    assert!(
        matches!(events.first(), Some(DataCompareEvent::Started { .. })),
        "{key}: first event {:?}",
        events.first()
    );
    match events.last() {
        Some(DataCompareEvent::Finished { result }) => result.clone(),
        other => panic!("{key}: compare ended with {other:?}"),
    }
}

fn counts(t: &TableDataDiff) -> [u64; 3] {
    [t.counts.inserts, t.counts.updates, t.counts.deletes]
}

fn applied(key: &str, events: &[DataApplyEvent]) -> DataApplyReport {
    assert!(
        matches!(events.first(), Some(DataApplyEvent::Started { .. })),
        "{key}: first event {:?}",
        events.first()
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DataApplyEvent::Recomparing)),
        "{key}: no recompare"
    );
    match events.last() {
        Some(DataApplyEvent::Finished { report }) => report.clone(),
        other => panic!("{key}: apply ended with {other:?}"),
    }
}

fn golden_path(key: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/golden/m8_data_{key}.sql"))
}

/// The whole exit check for one engine (module docs).
async fn data_sync(key: &'static str) {
    let live = Live::connect(key).await;
    live.setup().await;
    let has_hasher = default_dialects().chunk_hasher(live.spell.engine).is_some();

    // 1. Compare: events, counts, the 100k-row table in chunks of 10 000.
    let (events, took) = live
        .compare(live.request(live.pairs(), DataCompareOptions::default()))
        .await;
    let result = finished(key, &events);
    report(key, &format!("compare took {} ms", took.as_millis()));
    let tables_finished = events
        .iter()
        .filter(|e| matches!(e, DataCompareEvent::TableFinished { .. }))
        .count();
    assert_eq!(tables_finished, 4, "{key}");
    for t in &result.tables {
        report(
            key,
            &format!(
                "{}: {:?} {:?} skipped {} warnings {:?}",
                t.target.name, t.status, t.counts, t.chunks_skipped, t.warnings
            ),
        );
    }
    let [data, big, composite, nokey] = &result.tables[..] else {
        panic!("{key}: {} tables", result.tables.len());
    };
    assert_eq!(data.status, TableCompareStatus::Compared, "{key}");
    assert_eq!(counts(data), [2, 5, 2], "{key}: m8_data");
    assert_eq!(data.counts.same, 13, "{key}");
    assert_eq!(counts(big), [1, 1, 1], "{key}: m8_big");
    assert_eq!(big.counts.source_rows, 100_000, "{key}");
    assert_eq!(big.counts.target_rows, 100_000, "{key}");
    if has_hasher {
        // 10 chunks of 10 000: ids 500 and 50000 make two of them differ.
        assert!(
            big.chunks_skipped >= 8,
            "{key}: {} skipped",
            big.chunks_skipped
        );
        assert!(big.warnings.is_empty(), "{key}: {:?}", big.warnings);
    }
    assert_eq!(counts(composite), [0, 1, 0], "{key}: m8_composite");
    assert!(
        matches!(&nokey.status, TableCompareStatus::Refused { reason } if reason.contains("choose key columns")),
        "{key}: m8_nokey {:?}",
        nokey.status
    );
    let id = result.compare_id;

    // 2. Script previews: inserts and updates by default; with deletes it is destructive.
    let script = live
        .service
        .script(
            &DataScriptRequest {
                compare_id: id,
                options: DataScriptOptions::default(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(script.total_statements, 7 + 2 + 1, "{key}");
    assert!(!script.destructive && !script.truncated, "{key}");
    assert!(
        script
            .warnings
            .iter()
            .any(|w| w.contains("deletes are off")),
        "{key}: {:?}",
        script.warnings
    );
    let with_deletes = DataScriptOptions {
        deletes: true,
        ..DataScriptOptions::default()
    };
    let data_only = DataScriptOptions {
        pairs: vec![0],
        ..with_deletes.clone()
    };
    let preview = live
        .service
        .script(
            &DataScriptRequest {
                compare_id: id,
                options: data_only,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(preview.total_statements, 9, "{key}");
    assert!(preview.destructive, "{key}");
    let text = preview.statements.join("\n");
    report(key, &format!("m8_data script:\n{text}"));
    match std::fs::read_to_string(golden_path(key)) {
        Ok(golden) => pretty_assertions::assert_eq!(text.trim_end(), golden.trim_end(), "{key}"),
        Err(_) => report(key, "golden script pending (no tests/golden file yet)"),
    }

    // 3. Deletes need their confirmation.
    let refused = live
        .service
        .start_apply(DataApplyRequest {
            compare_id: id,
            options: DataApplyOptions {
                script: with_deletes.clone(),
                ..DataApplyOptions::default()
            },
        })
        .await
        .unwrap_err();
    assert_eq!(refused.category(), ErrorCategory::InvalidInput, "{key}");
    assert!(refused.to_string().contains("confirm"), "{key}: {refused}");

    // 4. Apply everything in batches of 4, then the re-compare is in sync.
    let handle = live
        .service
        .start_apply(DataApplyRequest {
            compare_id: id,
            options: DataApplyOptions {
                script: with_deletes,
                batch_statements: 4,
                confirm_deletes: true,
            },
        })
        .await
        .unwrap();
    let events = drain(handle.events, DataApplyEvent::is_terminal).await;
    let applied = applied(key, &events);
    report(key, &format!("apply: {applied:?}"));
    assert_eq!(applied.statements, 13, "{key}");
    assert_eq!(applied.committed_batches, 4, "{key}");
    assert!(applied.in_sync, "{key}: {:?}", applied.recompare);
    let recompare = applied.recompare.expect("re-compare");
    assert_eq!(recompare.tables.len(), 3, "{key}: the applied pairs");
    for t in &recompare.tables {
        assert!(t.counts.in_sync(), "{key}: {:?} {:?}", t.target, t.counts);
    }
    let data_report = applied.tables.iter().find(|t| t.pair == 0).unwrap();
    assert_eq!(
        (
            data_report.inserted,
            data_report.updated,
            data_report.deleted
        ),
        (2, 5, 2),
        "{key}"
    );

    // 5. A fresh compare agrees; then it is forgotten.
    let (events, _) = live
        .compare(live.request(live.pairs()[..3].to_vec(), DataCompareOptions::default()))
        .await;
    let again = finished(key, &events);
    for t in &again.tables {
        assert!(t.counts.in_sync(), "{key}: {:?} {:?}", t.target, t.counts);
    }
    assert!(live.service.forget(again.compare_id));
    assert!(!live.service.forget(again.compare_id));
    let gone = live
        .service
        .script(
            &DataScriptRequest {
                compare_id: again.compare_id,
                options: DataScriptOptions::default(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(gone.category(), ErrorCategory::NotFound, "{key}");

    // 6. Cancel: row by row in small chunks, cancelled right after it started.
    let handle = live
        .service
        .start_compare(live.request(
            vec![live.pairs()[1].clone()],
            DataCompareOptions {
                chunk_rows: 500,
                sample_rows: 20,
                use_chunk_hash: false,
            },
        ))
        .await
        .unwrap();
    let mut rx = handle.events;
    let first = rx.recv().await;
    assert!(
        matches!(first, Some(DataCompareEvent::Started { .. })),
        "{key}"
    );
    let cancelled_at = Instant::now();
    assert!(
        live.service.cancel(handle.id),
        "{key}: the compare is running"
    );
    let events = drain(rx, DataCompareEvent::is_terminal).await;
    assert!(
        matches!(events.last(), Some(DataCompareEvent::Cancelled { .. })),
        "{key}: {:?}",
        events.last()
    );
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(2),
        "{key}: cancel took {:?}",
        cancelled_at.elapsed()
    );

    live.cleanup().await;
    seeded(&live).await;
}

/// The conformance seeds' data pairs (`m8_src` → `m8_tgt`), read only.
async fn seeded(live: &Live) {
    let key = live.spell.key;
    if live.spell.engine == Engine::Sqlite {
        return;
    }
    let doc = fixtures();
    let m8 = &doc["m8"];
    let at = |container: &str, table: &str| match live.spell.engine {
        Engine::MySql | Engine::MariaDb => ObjectRef::table(Some(container), None, table),
        _ => ObjectRef::table(live.spell.catalog, Some(container), table),
    };
    let tables = ["m8_data", "m8_big", "m8_composite"];
    let pairs = tables
        .iter()
        .map(|t| TablePair {
            source: at("m8_src", t),
            target: at("m8_tgt", t),
            key: None,
            columns: None,
        })
        .collect();
    let (events, _) = live
        .compare(live.request(pairs, DataCompareOptions::default()))
        .await;
    let result = match events.last() {
        Some(DataCompareEvent::Finished { result } | DataCompareEvent::Failed { result, .. }) => {
            result.clone()
        }
        other => panic!("{key}: seeded compare ended with {other:?}"),
    };
    let missing = result
        .tables
        .iter()
        .any(|t| matches!(t.status, TableCompareStatus::Refused { .. }));
    if missing {
        assert!(
            !require_m8(),
            "{key}: the [m8] data seeds are missing: {:?}",
            result.tables
        );
        report(key, "seeded m8_src / m8_tgt data pairs: pending (no seed)");
        return;
    }
    for (t, name) in result.tables.iter().zip(tables) {
        let expected: Vec<u64> = m8["data_expected"][name]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| u64::try_from(v.as_integer().unwrap()).unwrap())
            .collect();
        assert_eq!(counts(t).to_vec(), expected, "{key}: seeded {name}");
    }
    report(key, "seeded m8_src / m8_tgt data pairs: as expected");
    live.service.forget(result.compare_id);
}

#[tokio::test]
async fn sqlite_data_sync() {
    data_sync("sqlite").await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures"]
async fn postgres_data_sync() {
    data_sync("postgres").await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures"]
async fn mysql_data_sync() {
    data_sync("mysql").await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures"]
async fn mariadb_data_sync() {
    data_sync("mariadb").await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures"]
async fn mssql_data_sync() {
    data_sync("mssql").await;
}
