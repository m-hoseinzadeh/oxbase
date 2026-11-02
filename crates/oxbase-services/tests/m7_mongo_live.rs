//! Live end-to-end tests of MongoDB through the services (mongo stream, docs/m7-contracts.md
//! §11 *browse*, *query*, *edit*, §5.3 import / export) against the `mongodb` fixture with the
//! M7 seed (`tests/docker/init/mongodb/02-m7.js`):
//!
//! - the object tree: `oxbase_test` → Collections / Views / Users / Roles, `m7_docs`'s indexes;
//! - `DocumentService::open` (`document_find` / `document_aggregate`): the column table of the
//!   contract, `document_at`, a `$group`, `m7_big` paged at random offsets, a cancel within 2 s;
//! - `DocumentService::apply` on `m7_edit` documents of the test, with a conflict;
//! - export of a document result to CSV and its import into a new collection.
//!
//! The dialect registry is built here with `oxbase_mongo::dialect::register` (the default
//! registry gets it at M7 integration). While the shell stream's language is not merged yet,
//! a one-statement stand-in takes its place, so builder runs (which never split) work.
//!
//! ```sh
//! scripts/fixtures-up.sh mongodb
//! cargo test -p oxbase-services --test m7_mongo_live -- --ignored --test-threads=1
//! ```

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arrow_array::cast::AsArray;
use arrow_array::types::{Int32Type, Int64Type};
use oxbase_dialect::script::{statement, trim_range};
use oxbase_dialect::{DialectRegistry, ScriptLanguage, StatementGuard};
use oxbase_io::{ColumnSpec, CsvReadOptions, CsvWriteOptions, ReadOptions, WriteOptions};
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_services::documents::DocumentService;
use oxbase_services::export::{
    ExportConfig, ExportDestination, ExportEvent, ExportRequest, ExportService, ExportSource,
};
use oxbase_services::history::HistoryService;
use oxbase_services::import::{
    ColumnMapping, ConversionOptions, ErrorPolicy, ImportConfig, ImportEvent, ImportRequest,
    ImportService, ImportSource, ImportTarget, TargetMode, TransactionPolicy,
};
use oxbase_services::query::{
    EditorId, QueryConfig, QueryService, RunEvent, RunHandle, RunId, RunOptions, RunTarget,
};
use oxbase_services::tree::{TreeNode, TreeNodeRef, TreeOptions, TreeService};
use oxbase_services::{
    CancellationToken, ConnectionManager, ConnectionProfile, ProvidedSecrets, SecretRef,
    SecretStore, SessionId, default_drivers,
};
use oxbase_sql::{Statement, StatementKind};
use oxbase_store::{
    MemoryHistoryRepository, MemorySecretStore, ProfileAddress, Secret, SecretSlot, TlsMode,
    TlsSettings,
};
use oxbase_types::document::{FieldChange, FieldValue};
use oxbase_types::{
    AggregateRequest, DocumentEdit, DocumentEditStatus, DocumentQuery, Engine, ErrorCategory,
    FindRequest, ObjectDef, ObjectKind, ObjectRef,
};
use serde_json::{Value as Json, json};

const DATABASE: &str = "oxbase_test";
const CANCEL_LIMIT: Duration = Duration::from_secs(2);

// ---- fixtures -----------------------------------------------------------------------------------

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn fixtures() -> toml::Table {
    let path = std::env::var_os("OXBASE_FIXTURES")
        .map(PathBuf::from)
        .map_or_else(|| root().join("tests/fixtures.toml"), |p| root().join(p));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.parse().expect("fixtures.toml parses")
}

fn mongo_fixture() -> toml::Table {
    fixtures()
        .get("engines")
        .and_then(|e| e.get("mongodb"))
        .and_then(toml::Value::as_table)
        .cloned()
        .expect("fixtures.toml: [engines.mongodb]")
}

fn string(t: &toml::Table, key: &str) -> String {
    t.get(key)
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("fixtures.toml: missing string {key}"))
        .to_owned()
}

fn token() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("{secs}x{}", std::process::id())
}

/// One statement per script: stands in for the mongo shell's language until the shell stream
/// is merged (builder runs never split their text; the history redaction has nothing to hide
/// in a request's shell text).
struct OneStatement;

impl ScriptLanguage for OneStatement {
    fn name(&self) -> &'static str {
        "mongo-shell"
    }

    fn editor_language(&self) -> &'static str {
        "javascript"
    }

    fn split(&self, text: &str) -> Vec<Statement> {
        trim_range(text, 0..text.len())
            .map(|range| statement(text, range, text.len(), StatementKind::Query))
            .into_iter()
            .collect()
    }

    fn redact_secrets<'a>(&self, text: &'a str) -> Cow<'a, str> {
        Cow::Borrowed(text)
    }

    fn guard(&self, statement: &Statement) -> StatementGuard {
        let _ = statement;
        StatementGuard::Allowed
    }
}

/// The MongoDB dialect cells, with the stand-in language while the shell is not merged.
fn dialects() -> DialectRegistry {
    let mut reg = DialectRegistry::new();
    oxbase_mongo::dialect::register(&mut reg);
    let shell_ready = std::panic::catch_unwind(|| {
        let _ = oxbase_mongo::shell::MongoShell.split("db.c.find()");
    })
    .is_ok();
    if !shell_ready {
        reg.register_script_language(Engine::MongoDb, Arc::new(OneStatement));
    }
    reg
}

// ---- the stack ----------------------------------------------------------------------------------

struct Live {
    session: SessionId,
    tree: Arc<TreeService>,
    query: Arc<QueryService>,
    documents: DocumentService,
    import: ImportService,
    export: ExportService,
    _spill: tempfile::TempDir,
}

impl Live {
    async fn new(sample_size: Option<usize>) -> Self {
        let f = mongo_fixture();
        let secrets = Arc::new(MemorySecretStore::new());
        let manager = Arc::new(ConnectionManager::new(
            default_drivers(),
            secrets.clone(),
            Arc::new(RusshTunnelProvider::new()),
        ));
        let mut p = ConnectionProfile::new(
            "live mongodb",
            Engine::MongoDb,
            ProfileAddress::Network {
                host: string(&f, "host"),
                port: Some(
                    f.get("port")
                        .and_then(toml::Value::as_integer)
                        .and_then(|p| u16::try_from(p).ok())
                        .expect("port"),
                ),
            },
        );
        p.user = Some(string(&f, "user"));
        p.database = Some(string(&f, "database"));
        let key = p.secret_key(SecretSlot::Password);
        secrets
            .set(&key, &Secret::new(string(&f, "password")))
            .await
            .unwrap();
        p.password = Some(SecretRef::Keychain { key });
        p.tls = TlsSettings {
            mode: TlsMode::Disable,
            ..TlsSettings::default()
        };
        p.connect_timeout_secs = Some(15);
        p.options
            .insert("auth_source".into(), string(&f, "auth_source"));
        if let Some(size) = sample_size {
            p.options.insert("sample_size".into(), size.to_string());
        }
        let handle = manager
            .open(&p, &ProvidedSecrets::default(), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("connect to the mongodb fixture: {e}"));
        let dialects = Arc::new(dialects());
        let spill = tempfile::tempdir().unwrap();
        let tree = Arc::new(TreeService::new(
            Arc::clone(&manager),
            Arc::clone(&dialects),
        ));
        let history = Arc::new(HistoryService::new(
            Arc::new(MemoryHistoryRepository::new()),
        ));
        let query = Arc::new(QueryService::new(
            Arc::clone(&manager),
            Arc::clone(&dialects),
            history,
            QueryConfig {
                spill_dir: spill.path().to_path_buf(),
                ..QueryConfig::default()
            },
        ));
        let documents = DocumentService::new(Arc::clone(&query), Arc::clone(&tree));
        let import = ImportService::new(
            Arc::clone(&manager),
            Arc::clone(&tree),
            Arc::clone(&dialects),
            ImportConfig::default(),
        );
        let export = ExportService::new(
            Arc::clone(&manager),
            Arc::clone(&query),
            Arc::clone(&tree),
            Arc::clone(&dialects),
            ExportConfig::default(),
        );
        Self {
            session: handle.id(),
            tree,
            query,
            documents,
            import,
            export,
            _spill: spill,
        }
    }

    fn target(&self, editor: &str) -> RunTarget {
        RunTarget {
            session: self.session,
            editor: EditorId::new(editor),
        }
    }

    async fn children(&self, node: TreeNodeRef) -> Vec<TreeNode> {
        self.tree
            .children(
                self.session,
                &node,
                &TreeOptions::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|e| panic!("children of {}: {e}", node.key()))
    }

    /// Starts `request` in `editor` and waits for its terminal event.
    async fn open(&self, editor: &str, request: DocumentQuery) -> (RunId, Vec<RunEvent>) {
        let handle = self
            .documents
            .open(
                self.target(editor),
                &request,
                RunOptions::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|e| panic!("document run: {e}"));
        let id = handle.id;
        (id, drain(handle).await)
    }

    /// Runs a JSON command request on the base session (clean-up).
    async fn command(&self, command: Json) {
        let text = json!({"type": "command", "database": DATABASE, "command": command}).to_string();
        let handle = self
            .query
            .run(
                self.target("cleanup"),
                &text,
                RunOptions {
                    record_history: false,
                    confirm_destructive: true,
                    ..RunOptions::default()
                },
            )
            .await;
        if let Ok(handle) = handle {
            let id = handle.id;
            drain(handle).await;
            let _ = self.query.close(id).await;
        }
    }
}

async fn drain(mut handle: RunHandle) -> Vec<RunEvent> {
    let mut events = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(120), handle.events.recv())
        .await
        .expect("run events")
    {
        let terminal = event.is_terminal();
        events.push(event);
        if terminal {
            break;
        }
    }
    events
}

fn finished(events: &[RunEvent]) {
    assert!(
        matches!(events.last(), Some(RunEvent::Finished { .. })),
        "{events:?}"
    );
}

fn collection(name: &str) -> ObjectRef {
    ObjectRef::table(Some(DATABASE), None, name)
}

fn labels(nodes: &[TreeNode]) -> Vec<&str> {
    nodes.iter().map(|n| n.label.as_str()).collect()
}

// ---- browse -------------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs the mongodb fixture with the M7 seed"]
async fn the_tree_shows_collections_views_users_roles_and_indexes() {
    let live = Live::new(None).await;
    let databases = live.children(TreeNodeRef::Connection).await;
    let db = databases
        .iter()
        .find(|n| n.label == DATABASE)
        .unwrap_or_else(|| panic!("{DATABASE} is not listed: {:?}", labels(&databases)));
    assert!(db.expandable);

    let folders = live.children(db.node.clone()).await;
    assert_eq!(labels(&folders), ["Collections", "Views", "Users", "Roles"]);

    let database = ObjectRef::database(DATABASE);
    let collections = live
        .children(TreeNodeRef::Folder {
            parent: database.clone(),
            kind: ObjectKind::Table,
        })
        .await;
    for name in ["m7_docs", "m7_big", "m7_edit"] {
        assert!(
            labels(&collections).contains(&name),
            "{:?}",
            labels(&collections)
        );
    }
    let views = live
        .children(TreeNodeRef::Folder {
            parent: database.clone(),
            kind: ObjectKind::View,
        })
        .await;
    assert!(labels(&views).contains(&"m7_view"), "{:?}", labels(&views));
    let roles = live
        .children(TreeNodeRef::Folder {
            parent: database.clone(),
            kind: ObjectKind::Role,
        })
        .await;
    assert_eq!(labels(&roles), ["m7_role"], "built-in roles are hidden");
    live.children(TreeNodeRef::Folder {
        parent: database,
        kind: ObjectKind::User,
    })
    .await;

    let docs = collection("m7_docs");
    let under = live
        .children(TreeNodeRef::Object {
            object: docs.clone(),
        })
        .await;
    assert_eq!(labels(&under), ["Indexes"]);
    let indexes = live
        .children(TreeNodeRef::Folder {
            parent: docs.clone(),
            kind: ObjectKind::Index,
        })
        .await;
    assert_eq!(indexes.len(), 5, "{:?}", labels(&indexes));

    let def = live
        .tree
        .load(live.session, &docs, CancellationToken::new())
        .await
        .unwrap();
    let ObjectDef::Table(def) = def else {
        panic!("a table def");
    };
    assert_eq!(def.primary_key.as_ref().unwrap().columns, ["_id"]);
    assert_eq!(def.column("n").unwrap().native_type, "long");
}

// ---- query --------------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs the mongodb fixture with the M7 seed"]
async fn document_find_gives_typed_columns_and_whole_documents() {
    let live = Live::new(Some(10)).await;
    let (run, events) = live
        .open(
            "find",
            DocumentQuery::Find(FindRequest::new(collection("m7_docs"))),
        )
        .await;
    finished(&events);
    let info = live.query.info(run).unwrap();
    assert_eq!(info.results.len(), 1);
    assert_eq!(info.results[0].rows, 12);
    assert_eq!(
        info.statements.len(),
        1,
        "a request is one statement: {:?}",
        info.statements
    );
    let columns = &info.results[0].columns;
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names.first(), Some(&"_id"));
    assert_eq!(names.last(), Some(&"$document"));
    assert!(!names.contains(&"only_late"), "{names:?}");
    let n = columns.iter().find(|c| c.name == "n").unwrap();
    assert_eq!(n.data_type, "Int64");
    assert_eq!(
        n.metadata.get("oxbase.bson_type").map(String::as_str),
        Some("long")
    );
    let mixed = columns.iter().find(|c| c.name == "mixed").unwrap();
    assert_eq!(
        mixed.metadata.get("oxbase.bson_type").map(String::as_str),
        Some("mixed")
    );
    assert_eq!(
        mixed
            .metadata
            .get("ARROW:extension:name")
            .map(String::as_str),
        Some("arrow.json")
    );

    let last = live.documents.document_at(run, 0, 11).await.unwrap();
    assert!(last.contains(r#""only_late":"late""#), "{last}");
    assert!(last.contains(r#""$numberLong":"12""#), "{last}");
    let first = live.documents.document_at(run, 0, 0).await.unwrap();
    assert!(
        first.contains(r#""$oid":"65a000000000000000000001""#),
        "{first}"
    );
    let past = live.documents.document_at(run, 0, 12).await.unwrap_err();
    assert_eq!(past.category(), ErrorCategory::NotFound);

    // An aggregate with $group.
    let (run, events) = live
        .open(
            "aggregate",
            DocumentQuery::Aggregate(AggregateRequest::new(
                collection("m7_big"),
                vec![json!({"$group": {"_id": null, "total": {"$sum": "$g"}}})],
            )),
        )
        .await;
    finished(&events);
    let page = live.query.page(run, 0, 0, 10).await.unwrap();
    assert_eq!(page.num_rows(), 1);
    let total = page.column(page.schema().index_of("total").unwrap());
    let total = total
        .as_primitive_opt::<Int32Type>()
        .map(|a| i64::from(a.value(0)))
        .or_else(|| total.as_primitive_opt::<Int64Type>().map(|a| a.value(0)))
        .expect("an integer total");
    assert_eq!(total, 9_900_000);
}

#[tokio::test]
#[ignore = "needs the mongodb fixture with the M7 seed"]
async fn m7_big_pages_at_random_offsets_and_cancels() {
    let live = Live::new(None).await;
    let mut find = FindRequest::new(collection("m7_big"));
    find.sort = Some(json!({"_id": 1}));
    let (run, events) = live.open("big", DocumentQuery::Find(find)).await;
    finished(&events);
    assert_eq!(live.query.info(run).unwrap().results[0].rows, 200_000);
    for offset in [0_u64, 1, 999, 1000, 77_777, 150_001, 199_999] {
        let page = live.query.page(run, 0, offset, 3).await.unwrap();
        let ids = page.column(0).as_primitive::<Int32Type>();
        let expected = i32::try_from(offset + 1).unwrap();
        assert_eq!(ids.value(0), expected, "offset {offset}");
        let g = page
            .column(page.schema().index_of("g").unwrap())
            .as_primitive::<Int32Type>();
        assert_eq!(g.value(0), expected % 100);
    }
    let _ = live.query.close(run).await;

    // Cancel while the server works: a $where that sleeps per document.
    let mut slow = FindRequest::new(collection("m7_big"));
    slow.filter = Some(json!({"$where": "function() { sleep(5); return true; }"}));
    let mut handle = live
        .documents
        .open(
            live.target("cancel"),
            &DocumentQuery::Find(slow),
            RunOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let run = handle.id;
    loop {
        let event = handle.events.recv().await.expect("events");
        if matches!(event, RunEvent::StatementStarted { .. }) {
            break;
        }
        assert!(!event.is_terminal(), "{event:?}");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    assert!(live.query.cancel(run));
    let events = drain(handle).await;
    assert!(
        matches!(events.last(), Some(RunEvent::Cancelled { .. })),
        "{events:?}"
    );
    assert!(started.elapsed() < CANCEL_LIMIT, "{:?}", started.elapsed());

    // The editor's session still works.
    let mut quick = FindRequest::new(collection("m7_big"));
    quick.filter = Some(json!({"g": 7}));
    quick.sort = Some(json!({"_id": 1}));
    quick.limit = Some(5);
    let (run, events) = live.open("cancel", DocumentQuery::Find(quick)).await;
    finished(&events);
    let page = live.query.page(run, 0, 0, 10).await.unwrap();
    let ids = page.column(0).as_primitive::<Int32Type>();
    let ids: Vec<i32> = (0..page.num_rows()).map(|i| ids.value(i)).collect();
    assert_eq!(ids, [7, 107, 207, 307, 407]);
}

// ---- edit ---------------------------------------------------------------------------------------

fn present(text: &str) -> FieldValue {
    FieldValue::Present {
        value: text.to_owned(),
    }
}

#[tokio::test]
#[ignore = "needs the mongodb fixture with the M7 seed"]
async fn document_apply_edits_and_detects_conflicts() {
    let live = Live::new(None).await;
    let edit = collection("m7_edit");
    let t = token();
    let id = format!(r#""t{t}-1""#);
    let apply = |edits: Vec<DocumentEdit>| {
        let live = &live;
        let edit = edit.clone();
        async move {
            live.documents
                .apply(live.session, &edit, &edits, CancellationToken::new())
                .await
                .expect("document_apply")
        }
    };

    let report = apply(vec![DocumentEdit::Insert {
        document: format!(r#"{{"_id": {id}, "name": "svc", "v": 1}}"#),
    }])
    .await;
    assert_eq!(report.applied(), 1, "{report:?}");
    let inserted = report.outcomes[0].document.clone().unwrap();

    let report = apply(vec![DocumentEdit::Update {
        id: id.clone(),
        changes: vec![
            FieldChange {
                path: "v".into(),
                previous: present("1"),
                value: present("2"),
            },
            FieldChange {
                path: "added".into(),
                previous: FieldValue::Missing,
                value: present(r#""yes""#),
            },
        ],
    }])
    .await;
    assert_eq!(report.applied(), 1, "{report:?}");
    let updated = report.outcomes[0].document.clone().unwrap();
    assert!(updated.contains(r#""added":"yes""#), "{updated}");

    // The text read before the update is stale now: replace and delete conflict.
    let report = apply(vec![DocumentEdit::Replace {
        id: id.clone(),
        previous: inserted.clone(),
        document: r#"{"name": "lost"}"#.into(),
    }])
    .await;
    assert_eq!(report.outcomes[0].status, DocumentEditStatus::Conflict);
    let now = live
        .documents
        .get(live.session, &edit, &id, CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(now, updated, "nothing was written");

    let report = apply(vec![
        DocumentEdit::Replace {
            id: id.clone(),
            previous: updated,
            document: r#"{"name": "replaced"}"#.into(),
        },
        DocumentEdit::Delete {
            id: id.clone(),
            previous: Some(inserted),
        },
    ])
    .await;
    assert_eq!(report.outcomes[0].status, DocumentEditStatus::Applied);
    assert_eq!(report.outcomes[1].status, DocumentEditStatus::Conflict);
    let replaced = report.outcomes[0].document.clone().unwrap();

    let report = apply(vec![DocumentEdit::Delete {
        id: id.clone(),
        previous: Some(replaced),
    }])
    .await;
    assert_eq!(report.applied(), 1, "{report:?}");
    let gone = live
        .documents
        .get(live.session, &edit, &id, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(gone, None);
}

// ---- import / export ----------------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs the mongodb fixture with the M7 seed"]
async fn a_collection_exports_to_csv_and_imports_into_a_new_collection() {
    let live = Live::new(None).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("m7_docs.csv");

    // Export: a find of m7_docs (name and n), through the result path.
    let mut find = FindRequest::new(collection("m7_docs"));
    find.projection = Some(json!({"name": 1, "n": 1}));
    find.sort = Some(json!({"_id": 1}));
    let (run, events) = live.open("export", DocumentQuery::Find(find)).await;
    finished(&events);
    let mut export = live
        .export
        .start(
            ExportRequest {
                source: ExportSource::Result {
                    run_id: run,
                    result_index: 0,
                },
                options: WriteOptions::Csv(CsvWriteOptions::default()),
            },
            ExportDestination {
                path: file.clone(),
                overwrite: false,
            },
        )
        .await
        .expect("export starts");
    let mut done = None;
    while let Some(event) = export.events.recv().await {
        if event.is_terminal() {
            done = Some(event);
            break;
        }
    }
    let Some(ExportEvent::Finished { report }) = done else {
        panic!("export: {done:?}");
    };
    assert_eq!(report.sink.rows, 12);

    // Import: the file into a new collection (create mode, no transaction).
    let name = format!("m7imp_t{}", token());
    let target = collection(&name);
    let source = ImportSource {
        path: file.clone(),
        options: ReadOptions::Csv(CsvReadOptions::default()),
    };
    let preview = live
        .import
        .preview(&source, CancellationToken::new())
        .await
        .expect("preview");
    let columns: Vec<ColumnSpec> = preview
        .columns
        .iter()
        .map(|c| ColumnSpec::new(&c.name, c.data_type.clone()))
        .collect();
    let mapping = preview
        .columns
        .iter()
        .map(|c| ColumnMapping {
            source: c.index,
            target: match c.name.as_str() {
                "_id" => Some("source_id".to_owned()),
                "name" | "n" => Some(c.name.clone()),
                _ => None,
            },
            data_type: None,
        })
        .collect();
    let mut request = ImportRequest {
        options: source.options.clone(),
        columns,
        mapping,
        target: ImportTarget {
            session: live.session,
            table: target.clone(),
            mode: TargetMode::Truncate,
        },
        primary_key: Vec::new(),
        conversion: ConversionOptions::default(),
        errors: ErrorPolicy::default(),
        transaction: TransactionPolicy::default(),
    };
    // Modes that would empty or replace a collection are refused for a loader without a
    // transaction.
    for mode in [
        TargetMode::Truncate,
        TargetMode::DeleteAll,
        TargetMode::Replace,
    ] {
        request.target.mode = mode;
        let err = live
            .import
            .plan(&source, &request, CancellationToken::new())
            .await
            .expect_err("refused");
        assert!(
            matches!(
                err.category(),
                ErrorCategory::Unsupported | ErrorCategory::NotFound
            ),
            "{mode:?}: {err}"
        );
    }
    request.target.mode = TargetMode::Create;
    let plan = live
        .import
        .plan(&source, &request, CancellationToken::new())
        .await
        .expect("plan");
    assert!(plan.creates_table);
    assert!(plan.ddl.is_empty(), "the loader creates the collection");
    let mut handle = live.import.start(source, request).await.expect("import");
    let mut last = None;
    while let Some(event) = handle.events.recv().await {
        if event.is_terminal() {
            last = Some(event);
            break;
        }
    }
    let result = match last {
        Some(ImportEvent::Finished { report }) => Ok(report),
        other => Err(format!("{other:?}")),
    };

    // Read the new collection back before cleaning up.
    let check = async {
        let report = result?;
        if report.rows_committed != 12 || !report.table_created {
            return Err(format!("{report:?}"));
        }
        let mut back = FindRequest::new(target.clone());
        back.sort = Some(json!({"name": 1}));
        let (run, events) = live.open("readback", DocumentQuery::Find(back)).await;
        finished(&events);
        let page = live.query.page(run, 0, 0, 100).await.unwrap();
        if page.num_rows() != 12 {
            return Err(format!("{} rows read back", page.num_rows()));
        }
        let schema = page.schema();
        let names = page
            .column(schema.index_of("name").unwrap())
            .as_string::<i32>();
        let n = page.column(schema.index_of("n").unwrap());
        let number = |i: usize| {
            n.as_primitive_opt::<Int64Type>()
                .map(|a| a.value(i))
                .or_else(|| {
                    n.as_primitive_opt::<Int32Type>()
                        .map(|a| i64::from(a.value(i)))
                })
        };
        for i in 0..12 {
            let expected_name = format!("doc-{:02}", i + 1);
            let expected_n = i64::try_from(i + 1).unwrap();
            if names.value(i) != expected_name || number(i) != Some(expected_n) {
                return Err(format!("row {i}: {} / {:?}", names.value(i), number(i)));
            }
        }
        let source_id = page
            .column(schema.index_of("source_id").unwrap())
            .as_string::<i32>();
        if source_id.value(0) != "65a000000000000000000001" {
            return Err(format!("source_id {}", source_id.value(0)));
        }
        Ok::<(), String>(())
    }
    .await;
    live.command(json!({"drop": name})).await;
    if let Err(e) = check {
        panic!("round trip: {e}");
    }
}
